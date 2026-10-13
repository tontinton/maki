use std::mem;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use maki_agent::command::CustomCommand;
use maki_agent::{McpPromptInfo, McpSnapshotReader};
use maki_lua::{LuaCommandInfo, LuaCommandReader, Missing, Permission, PluginReach};
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Matcher, Nucleo, Utf32String};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::text_buffer::TextBuffer;
use crate::theme;

const TICK_TIMEOUT_MS: u64 = 10;
pub(crate) const USER_ONLY_SUFFIX: &str = " can only be run by you";
const STARTS_TURNS: PluginAccess = PluginAccess::Needs(&[Permission::Agent]);
/// A prompt starts a turn and adds to the transcript, like `maki.session.prompt`.
pub const SUBMITS_PROMPT: PluginAccess =
    PluginAccess::Needs(&[Permission::Agent, Permission::SessionWrite]);
const WRITES_SESSION: PluginAccess = PluginAccess::Needs(&[Permission::SessionWrite]);
const CHANGES_SETTINGS: PluginAccess = PluginAccess::Needs(&[Permission::SessionSettings]);

/// What a plugin must hold to run a command through `maki.api.run_command`.
/// Typing a command is the user saying yes, so none of this applies there.
///
/// Mirror the guard on the Lua API that does the same thing: looser is a side
/// door around it, stricter only sends plugins to the API instead. That is why
/// `/model` and `/fast` need `session_settings`, as `maki.model.set` does.
/// So do `/cd` and `/workflow`: no API mirrors them, but they change where the
/// session's tools run and which tools it has.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PluginAccess {
    /// Only what changes the view, never how a session runs or what it holds.
    Open,
    /// Every one of them.
    Needs(&'static [Permission]),
    /// These move a security boundary: what runs without asking, which config
    /// loads, which code is installed. No grant unlocks them.
    UserOnly,
}

impl PluginAccess {
    /// Why {caller} may not do {what}, or `None` when it may. Every refusal a
    /// plugin gets for something the user could have typed reads this way.
    pub(crate) fn refusal(self, what: &str, caller: &PluginReach) -> Option<String> {
        match self {
            Self::Open => None,
            Self::Needs(needed) => needed
                .iter()
                .find(|&&perm| !caller.is_allowed(perm))
                .map(|&perm| Missing::Permission(perm).refusal(what)),
            Self::UserOnly => Some(format!("{what}{USER_ONLY_SUFFIX}")),
        }
    }
}

pub struct BuiltinCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub max_args: usize,
    /// A typed `!` filters the palette down to commands that read one, so no
    /// handler can be reached by a bang it ignores.
    pub bang: bool,
    pub plugin_access: PluginAccess,
}

pub const BUILTIN_COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand {
        name: "/compact",
        description: "Summarize and compact conversation history (optional guidance)",
        max_args: usize::MAX,
        bang: false,
        plugin_access: SUBMITS_PROMPT,
    },
    BuiltinCommand {
        name: "/new",
        description: "Start a new session",
        max_args: 0,
        bang: false,
        plugin_access: WRITES_SESSION,
    },
    BuiltinCommand {
        name: "/fork",
        description: "Fork this session and switch to the copy",
        max_args: 0,
        bang: false,
        plugin_access: WRITES_SESSION,
    },
    BuiltinCommand {
        name: "/help",
        description: "Show keybindings",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::Open,
    },
    BuiltinCommand {
        name: "/usage",
        description: "Show token usage breakdown",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::Open,
    },
    BuiltinCommand {
        name: "/queue",
        description: "Remove items from queue",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::Open,
    },
    BuiltinCommand {
        name: "/model",
        description: "Switch model",
        max_args: 0,
        bang: false,
        plugin_access: CHANGES_SETTINGS,
    },
    BuiltinCommand {
        name: "/theme",
        description: "Switch color theme",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::Open,
    },
    BuiltinCommand {
        name: "/mcp",
        description: "Configure MCP servers",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::Open,
    },
    BuiltinCommand {
        name: "/login",
        description: "Authenticate with an LLM provider",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::Open,
    },
    BuiltinCommand {
        name: "/cd",
        description: "Change working directory",
        max_args: 1,
        bang: false,
        plugin_access: CHANGES_SETTINGS,
    },
    BuiltinCommand {
        name: "/btw",
        description: "Ask a quick question (no tools, no history pollution)",
        max_args: usize::MAX,
        bang: false,
        plugin_access: STARTS_TURNS,
    },
    BuiltinCommand {
        name: "/yolo",
        description: "Toggle YOLO mode (skip all permission prompts)",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::UserOnly,
    },
    BuiltinCommand {
        name: "/fast",
        description: "Toggle fast mode (Anthropic Opus or Codex subscription models)",
        max_args: 0,
        bang: false,
        plugin_access: CHANGES_SETTINGS,
    },
    BuiltinCommand {
        name: "/workflow",
        description: "Toggle workflow mode (task callable inside code_execution)",
        max_args: 0,
        bang: false,
        plugin_access: CHANGES_SETTINGS,
    },
    BuiltinCommand {
        name: "/exit",
        description: "Exit the application",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::Open,
    },
    BuiltinCommand {
        name: "/reload",
        description: "Reload plugins and config",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::Open,
    },
    BuiltinCommand {
        name: "/trust",
        description: "Trust this folder and load its shared project config",
        max_args: 0,
        bang: false,
        plugin_access: PluginAccess::UserOnly,
    },
    BuiltinCommand {
        name: "/packupdate",
        description: "Update packages (++lockfile, ! skips review)",
        max_args: 2,
        bang: true,
        plugin_access: PluginAccess::UserOnly,
    },
    BuiltinCommand {
        name: "/packdel",
        description: "Remove undeclared packages (++all, or a name)",
        max_args: 1,
        bang: true,
        plugin_access: PluginAccess::UserOnly,
    },
];

pub struct ParsedCommand {
    pub name: String,
    /// Already trimmed, so empty means no args and handlers never re-check
    /// whitespace.
    pub args: String,
    pub bang: bool,
}

pub enum CommandAction {
    Consumed,
    Execute(ParsedCommand),
    Complete(String),
    Passthrough,
}

#[derive(Clone)]
enum CommandType {
    Builtin(&'static BuiltinCommand),
    Custom(usize),
    McpPrompt(usize),
    Lua(usize),
}

struct CommandItem {
    name: String,
    max_args: usize,
    command_type: CommandType,
}

struct Match {
    command_type: CommandType,
    indices: Vec<u32>,
}

/// Every public method takes the input and catches up with it first. Lots of
/// paths change the text without a keypress (a chat switch, `$EDITOR`, history
/// recall). If each had to tell the palette, one would forget sooner or later,
/// and `Enter` would run a command the box no longer shows.
pub struct CommandPalette {
    /// `Esc` closes the list but leaves this alone, so the palette stays shut
    /// until the text changes.
    synced_version: Option<u64>,
    selected: usize,
    scroll_offset: usize,
    filtered: Vec<Match>,
    custom: Arc<[CustomCommand]>,
    mcp_reader: McpSnapshotReader,
    mcp_prompts: Vec<McpPromptInfo>,
    mcp_generation: u64,
    lua_reader: LuaCommandReader,
    lua_commands: Vec<LuaCommandInfo>,
    lua_generation: u64,
    nucleo: Nucleo<CommandItem>,
    matcher: Matcher,
    current_arg_count: usize,
    current_bang: bool,
}

impl CommandPalette {
    pub fn new(
        custom_commands: Arc<[CustomCommand]>,
        mcp_reader: McpSnapshotReader,
        lua_reader: LuaCommandReader,
    ) -> Self {
        let snap = mcp_reader.load();
        let mcp_generation = snap.generation;
        let prompts = snap.prompts.clone();

        let lua_snap = lua_reader.load();
        let lua_generation = lua_snap.generation;
        let lua_commands = lua_snap.commands.clone();

        let nucleo = Self::build_nucleo(&custom_commands, &prompts, &lua_commands);
        Self {
            synced_version: None,
            selected: 0,
            scroll_offset: 0,
            filtered: Vec::new(),
            custom: custom_commands,
            mcp_reader,
            mcp_prompts: prompts,
            mcp_generation,
            lua_reader,
            lua_commands,
            lua_generation,
            nucleo,
            matcher: Matcher::new(Config::DEFAULT),
            current_arg_count: 0,
            current_bang: false,
        }
    }

    /// Every command the palette knows, in display order. The one place that
    /// enumerates the four sources: matching and name lookup both read it.
    fn items<'a>(
        custom_commands: &'a [CustomCommand],
        mcp_prompts: &'a [McpPromptInfo],
        lua_commands: &'a [LuaCommandInfo],
    ) -> impl Iterator<Item = CommandItem> + 'a {
        let builtins = BUILTIN_COMMANDS.iter().map(|cmd| CommandItem {
            name: cmd.name.to_string(),
            max_args: cmd.max_args,
            command_type: CommandType::Builtin(cmd),
        });
        let custom = custom_commands
            .iter()
            .enumerate()
            .map(|(i, cmd)| CommandItem {
                name: cmd.display_name(),
                max_args: if cmd.has_args() { usize::MAX } else { 0 },
                command_type: CommandType::Custom(i),
            });
        let prompts = mcp_prompts.iter().enumerate().map(|(i, p)| CommandItem {
            name: format!("/{}", p.display_name),
            max_args: if p.arguments.is_empty() {
                0
            } else {
                usize::MAX
            },
            command_type: CommandType::McpPrompt(i),
        });
        let lua = lua_commands.iter().enumerate().map(|(i, cmd)| CommandItem {
            name: cmd.name.to_string(),
            max_args: cmd.max_args,
            command_type: CommandType::Lua(i),
        });
        builtins.chain(custom).chain(prompts).chain(lua)
    }

    fn build_nucleo(
        custom_commands: &[CustomCommand],
        mcp_prompts: &[McpPromptInfo],
        lua_commands: &[LuaCommandInfo],
    ) -> Nucleo<CommandItem> {
        let nucleo = Nucleo::new(Config::DEFAULT, Arc::new(|| {}), None, 1);
        let injector = nucleo.injector();

        for item in Self::items(custom_commands, mcp_prompts, lua_commands) {
            injector.push(item, |item, cols| {
                cols[0] = Utf32String::from(item.name.as_str());
            });
        }

        nucleo
    }

    pub fn handle_key(&mut self, key: KeyEvent, input: &TextBuffer) -> CommandAction {
        if !self.is_active(input) {
            return CommandAction::Passthrough;
        }
        match key.code {
            KeyCode::Up => {
                self.move_up();
                CommandAction::Consumed
            }
            KeyCode::Down => {
                self.move_down();
                CommandAction::Consumed
            }
            KeyCode::Esc => {
                self.close();
                CommandAction::Consumed
            }
            KeyCode::Enter => match self.confirm(input.value()) {
                Some(cmd) => {
                    self.close();
                    CommandAction::Execute(cmd)
                }
                None => CommandAction::Consumed,
            },
            KeyCode::Tab => {
                if let Some(item) = self.filtered.get(self.selected) {
                    let mut text = self.item_name(item);
                    if self.current_bang {
                        text.push('!');
                    }
                    if self.item_has_args(item) {
                        text.push(' ');
                    }
                    CommandAction::Complete(text)
                } else {
                    CommandAction::Consumed
                }
            }
            _ => CommandAction::Passthrough,
        }
    }

    /// The command Enter would run on {input}, looked up before it does.
    pub fn enter_target(&mut self, input: &TextBuffer) -> Option<String> {
        if !self.is_active(input) {
            return None;
        }
        self.filtered.get(self.selected).map(|m| self.item_name(m))
    }

    pub fn is_active(&mut self, input: &TextBuffer) -> bool {
        self.follow(input);
        self.is_open()
    }

    fn is_open(&self) -> bool {
        !self.filtered.is_empty()
    }

    /// The box keeps one buffer for life and its version only climbs, so a
    /// version we have seen means text we already matched.
    fn follow(&mut self, input: &TextBuffer) {
        let version = Some(input.version());
        if self.synced_version != version {
            self.synced_version = version;
            self.sync(input.value());
        }
    }

    fn sync(&mut self, input: &str) {
        let mcp_snap = self.mcp_reader.load();
        let lua_snap = self.lua_reader.load();
        if mcp_snap.generation != self.mcp_generation || lua_snap.generation != self.lua_generation
        {
            self.mcp_generation = mcp_snap.generation;
            self.mcp_prompts = mcp_snap.prompts.clone();
            self.lua_generation = lua_snap.generation;
            self.lua_commands = lua_snap.commands.clone();
            self.nucleo = Self::build_nucleo(&self.custom, &self.mcp_prompts, &self.lua_commands);
        }
        let Some(stripped) = input.strip_prefix('/') else {
            self.filtered.clear();
            self.current_arg_count = 0;
            self.current_bang = false;
            return;
        };

        let parts: Vec<&str> = stripped.split_whitespace().collect();
        let raw_word = parts.first().copied().unwrap_or(stripped);
        let (cmd_word, bang) = raw_word
            .strip_suffix('!')
            .map_or((raw_word, false), |word| (word, true));
        self.current_bang = bang;
        let trailing_space = stripped.ends_with(char::is_whitespace);

        self.current_arg_count = if trailing_space {
            parts.len()
        } else {
            parts.len().saturating_sub(1)
        };

        self.nucleo.pattern.reparse(
            0,
            cmd_word,
            CaseMatching::Ignore,
            Normalization::Smart,
            false,
        );

        self.tick();

        let typed = format!("/{cmd_word}");
        match self
            .filtered
            .iter()
            .position(|item| self.item_name(item).eq_ignore_ascii_case(&typed))
        {
            // A full name beats any fuzzy neighbor, so `/model` cannot run
            // `/models`.
            Some(index) => {
                let exact = self.filtered.remove(index);
                self.filtered.insert(0, exact);
                self.selected = 0;
            }
            // The command exists but this input ruled it out: too many args, or
            // a bang it does not read. Its fuzzy neighbors are still here and
            // Enter would run one, so `/cd a b` would launch `/packupdate`.
            None if self.find_item(&typed).is_some() => self.filtered.clear(),
            None => {}
        }
    }

    fn tick(&mut self) {
        loop {
            let status = self.nucleo.tick(TICK_TIMEOUT_MS);
            if status.changed {
                self.refresh_matches();
            }
            if !status.running {
                break;
            }
        }
    }

    fn refresh_matches(&mut self) {
        let snapshot = self.nucleo.snapshot();
        let pattern = snapshot.pattern();
        let has_pattern = !pattern.column_pattern(0).atoms.is_empty();

        self.filtered.clear();
        let count = snapshot.matched_item_count();
        for item in snapshot.matched_items(0..count) {
            let cmd_item = &item.data;
            let col = &item.matcher_columns[0];

            if self.current_arg_count > cmd_item.max_args {
                continue;
            }
            if self.current_bang
                && !matches!(cmd_item.command_type, CommandType::Builtin(command) if command.bang)
            {
                continue;
            }

            let indices = if has_pattern {
                let mut indices_buf = vec![];
                pattern.column_pattern(0).indices(
                    col.slice(..),
                    &mut self.matcher,
                    &mut indices_buf,
                );
                indices_buf
            } else {
                Vec::new()
            };

            self.filtered.push(Match {
                command_type: cmd_item.command_type.clone(),
                indices,
            });
        }

        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    pub fn close(&mut self) {
        self.filtered.clear();
        self.scroll_offset = 0;
        self.current_arg_count = 0;
        self.current_bang = false;
    }

    fn ensure_visible(&mut self, visible: usize) {
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + visible {
            self.scroll_offset = self.selected + 1 - visible;
        }
        // A filter that shrinks the list can leave the offset deep in empty
        // space, where the selection alone would pin a one-row window.
        self.scroll_offset = self
            .scroll_offset
            .min(self.filtered.len().saturating_sub(visible));
    }

    fn move_up(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.filtered.len() - 1
        } else {
            self.selected - 1
        };
    }

    fn move_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = if self.selected == self.filtered.len() - 1 {
            0
        } else {
            self.selected + 1
        };
    }

    fn item_name(&self, m: &Match) -> String {
        match &m.command_type {
            CommandType::Builtin(cmd) => cmd.name.to_string(),
            CommandType::Custom(i) => self.custom[*i].display_name(),
            CommandType::McpPrompt(i) => format!("/{}", self.mcp_prompts[*i].display_name),
            CommandType::Lua(i) => self.lua_commands[*i].name.to_string(),
        }
    }

    fn item_has_args(&self, m: &Match) -> bool {
        match &m.command_type {
            CommandType::Builtin(cmd) => cmd.max_args > 0,
            CommandType::Custom(i) => self.custom[*i].has_args(),
            CommandType::McpPrompt(i) => !self.mcp_prompts[*i].arguments.is_empty(),
            CommandType::Lua(i) => self.lua_commands[*i].max_args > 0,
        }
    }

    fn item_description(&self, m: &Match) -> &str {
        match &m.command_type {
            CommandType::Builtin(cmd) => cmd.description,
            CommandType::Custom(i) => &self.custom[*i].description,
            CommandType::McpPrompt(i) => &self.mcp_prompts[*i].description,
            CommandType::Lua(i) => &self.lua_commands[*i].description,
        }
    }

    fn confirm(&self, input: &str) -> Option<ParsedCommand> {
        let item = self.filtered.get(self.selected)?;
        let name = self.item_name(item);
        let args = input
            .strip_prefix('/')
            .and_then(|s| s.split_once(char::is_whitespace))
            .map(|(_, a)| a.trim())
            .unwrap_or("");
        Some(ParsedCommand {
            name,
            args: args.to_string(),
            bang: self.current_bang,
        })
    }

    /// Exact name lookup, case-insensitive like typing, but never fuzzy: an
    /// alias names one command on purpose, and a typo should report itself
    /// instead of running the closest neighbor.
    fn find_item(&self, name: &str) -> Option<CommandItem> {
        Self::items(&self.custom, &self.mcp_prompts, &self.lua_commands)
            .find(|item| item.name.eq_ignore_ascii_case(name))
    }

    /// Hands back the registered spelling, because that is the only one
    /// [`crate::app::App`] dispatches on. `Err` goes back to the plugin as is.
    pub fn resolve_for_plugin(&self, name: &str, caller: &PluginReach) -> Result<String, String> {
        let item = self
            .find_item(name)
            .ok_or_else(|| format!("unknown command '{name}'"))?;
        match self.plugin_refusal(&item, caller) {
            Some(refusal) => Err(refusal),
            None => Ok(item.name),
        }
    }

    fn plugin_refusal(&self, item: &CommandItem, caller: &PluginReach) -> Option<String> {
        let access = match item.command_type {
            CommandType::Builtin(cmd) => cmd.plugin_access,
            CommandType::Custom(_) | CommandType::McpPrompt(_) => SUBMITS_PROMPT,
            CommandType::Lua(i) => {
                return caller
                    .first_missing(&self.lua_commands[i].reach)
                    .map(|missing| missing.refusal(&item.name));
            }
        };
        access.refusal(&item.name, caller)
    }

    pub fn find_custom_command(&self, display_name: &str) -> Option<&CustomCommand> {
        self.custom
            .iter()
            .find(|c| c.display_name() == display_name)
    }

    pub fn find_mcp_prompt(&self, slash_name: &str) -> Option<&McpPromptInfo> {
        let name = slash_name.strip_prefix('/')?;
        self.mcp_prompts.iter().find(|p| p.display_name == name)
    }

    pub fn find_lua_command(&self, name: &str) -> Option<&LuaCommandInfo> {
        self.lua_commands.iter().find(|c| c.name.as_ref() == name)
    }

    pub fn lua_commands(&self) -> &[LuaCommandInfo] {
        &self.lua_commands
    }

    pub fn view(
        &mut self,
        frame: &mut Frame,
        input_area: Rect,
        input: &TextBuffer,
    ) -> Option<Rect> {
        self.follow(input);
        self.render(frame, input_area)
    }

    fn render(&mut self, frame: &mut Frame, input_area: Rect) -> Option<Rect> {
        if !self.is_open() {
            return None;
        }

        let popup_height = (self.filtered.len() as u16).min(input_area.y);
        if popup_height == 0 {
            return None;
        }
        self.ensure_visible(popup_height as usize);
        let start = self.scroll_offset;
        let filtered = &self.filtered;
        let end = (start + popup_height as usize).min(filtered.len());

        const GAP: usize = 2;
        let max_name = filtered
            .iter()
            .map(|item| self.item_name(item).len())
            .max()
            .unwrap_or(0);
        let max_desc = filtered
            .iter()
            .map(|item| self.item_description(item).len())
            .max()
            .unwrap_or(0);
        const PAD: usize = 1;
        let popup_width = (PAD + max_name + GAP + max_desc + PAD) as u16;

        let popup = Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(popup_height),
            width: popup_width.min(input_area.width),
            height: popup_height,
        };

        let t = theme::current();
        let lines: Vec<Line> = filtered[start..end]
            .iter()
            .enumerate()
            .map(|(row, m)| {
                let i = start + row;
                let name = self.item_name(m);
                let desc = self.item_description(m);
                let selected = i == self.selected;
                let name_pad = max_name - name.len() + GAP;

                if selected {
                    let s = t.item_selected;
                    let highlighted_name = self.build_highlighted_spans(&name, &m.indices, s);
                    let mut spans = vec![Span::styled(" ".repeat(PAD), s)];
                    spans.extend(highlighted_name);
                    spans.push(Span::styled(" ".repeat(name_pad), s));
                    spans.push(Span::styled(desc, s));
                    spans.push(Span::styled(" ".repeat(PAD), s));
                    Line::from(spans)
                } else {
                    let highlighted_name = self.build_highlighted_spans(&name, &m.indices, t.item);
                    let mut spans = vec![Span::raw(" ".repeat(PAD))];
                    spans.extend(highlighted_name);
                    spans.push(Span::raw(" ".repeat(name_pad)));
                    spans.push(Span::styled(desc, t.item_desc));
                    spans.push(Span::raw(" ".repeat(PAD)));
                    Line::from(spans)
                }
            })
            .collect();

        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).style(Style::new().bg(t.background)),
            popup,
        );

        Some(popup)
    }

    fn build_highlighted_spans(&self, text: &str, indices: &[u32], base: Style) -> Vec<Span<'_>> {
        if indices.is_empty() {
            return vec![Span::styled(text.to_string(), base)];
        }

        let t = theme::current();
        let highlight = base
            .fg(t.accent.fg.unwrap_or_default())
            .add_modifier(Modifier::BOLD);

        let mut spans = Vec::new();
        let mut in_match = false;
        let mut run = String::new();

        for (i, ch) in text.chars().enumerate() {
            let is_match = indices.binary_search(&(i as u32)).is_ok();
            if is_match != in_match && !run.is_empty() {
                spans.push(Span::styled(
                    mem::take(&mut run),
                    if in_match { highlight } else { base },
                ));
            }
            in_match = is_match;
            run.push(ch);
        }

        if !run.is_empty() {
            spans.push(Span::styled(run, if in_match { highlight } else { base }));
        }

        spans
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use maki_agent::{McpPromptArg, McpSnapshot};
    use maki_lua::PluginPermissions;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    fn empty_snapshot() -> McpSnapshotReader {
        McpSnapshotReader::empty()
    }

    fn synced(input: &str) -> CommandPalette {
        let mut p = CommandPalette::new(Arc::from([]), empty_snapshot(), LuaCommandReader::empty());
        p.sync(input);
        p
    }

    fn synced_with_custom(input: &str, custom: Arc<[CustomCommand]>) -> CommandPalette {
        let mut p = CommandPalette::new(custom, empty_snapshot(), LuaCommandReader::empty());
        p.sync(input);
        p
    }

    fn sample_custom() -> Arc<[CustomCommand]> {
        Arc::from([
            CustomCommand {
                name: "review".into(),
                description: "Code review".into(),
                content: "Review $ARGUMENTS".into(),
                scope: maki_agent::command::CommandScope::Project,
                accepts_args: true,
            },
            CustomCommand {
                name: "fix".into(),
                description: "Quick fix".into(),
                content: "Fix the code".into(),
                scope: maki_agent::command::CommandScope::User,
                accepts_args: false,
            },
        ])
    }

    /// A builtin is open to plugins only when it neither reads nor changes a
    /// session's content or settings: it opens host UI the user then drives,
    /// or acts on the process. A new builtin lands outside this list, so
    /// opening one is a decision someone writes down here, never a default.
    const OPEN_TO_PLUGINS: &[&str] = &[
        "/help", "/usage", "/queue", "/theme", "/mcp", "/login", "/exit", "/reload",
    ];

    #[test]
    fn only_commands_that_never_touch_a_session_are_open_to_plugins() {
        let open: Vec<&str> = BUILTIN_COMMANDS
            .iter()
            .filter(|cmd| cmd.plugin_access == PluginAccess::Open)
            .map(|cmd| cmd.name)
            .collect();
        assert_eq!(open, OPEN_TO_PLUGINS);
    }

    #[test]
    fn slash_shows_builtins_plus_extras() {
        let builtin_count = synced("/").filtered.len();
        assert!(builtin_count > 0);

        let with_custom = synced_with_custom("/", sample_custom());
        assert_eq!(with_custom.filtered.len(), builtin_count + 2);

        let with_prompts = synced_with_prompts("/");
        assert_eq!(with_prompts.filtered.len(), builtin_count + 2);
    }

    #[test]
    fn close_deactivates() {
        let mut p = synced("/");
        p.close();
        assert!(!p.is_open());
    }

    #[test_case("/mp", true ; "compact_substring")]
    #[test_case("/ew", true ; "lowercase_substring")]
    #[test_case("/EW", true ; "uppercase_substring")]
    #[test_case("/zzz", false ; "no_match")]
    fn filter_by_substring(input: &str, expect_active: bool) {
        let p = synced(input);
        assert_eq!(p.is_open(), expect_active);
    }

    #[test]
    fn filter_custom_by_substring() {
        let p = synced_with_custom("/review", sample_custom());
        assert!(p.is_open());
        assert_eq!(p.filtered.len(), 1);
        assert!(matches!(p.filtered[0].command_type, CommandType::Custom(0)));
    }

    #[test]
    fn navigation_wraps() {
        let mut p = synced("/");
        p.move_up();
        assert_eq!(p.selected, p.filtered.len() - 1);
        p.move_down();
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn confirm_when_inactive_returns_none() {
        let p = CommandPalette::new(Arc::from([]), empty_snapshot(), LuaCommandReader::empty());
        assert!(p.confirm("").is_none());
    }

    #[test]
    fn sync_clamps_selected() {
        let mut p = synced("/");
        p.selected = 100;
        p.sync("/");
        assert_eq!(p.selected, p.filtered.len() - 1);
    }

    #[test]
    fn sync_filters_on_first_word_only() {
        let p = synced("/cd ~/foo");
        assert!(p.is_open());
        let name = p.item_name(&p.filtered[0]);
        assert_eq!(name, "/cd");
    }

    #[test_case("/help ", false     ; "zero_arg_help_with_space")]
    #[test_case("/compact ", true   ; "compact_stays_active_with_space")]
    #[test_case("/cd ", true        ; "one_arg_cmd_with_space")]
    #[test_case("/cd ~/foo", true   ; "one_arg_cmd_mid_arg")]
    #[test_case("/cd  ~/foo", true  ; "one_arg_cmd_double_space")]
    #[test_case("/cd ~/foo ", false ; "one_arg_cmd_second_space")]
    #[test_case("/btw hello world", true ; "btw_stays_active_with_many_args")]
    fn sync_respects_nargs(input: &str, expect_active: bool) {
        let p = synced(input);
        assert_eq!(p.is_open(), expect_active);
    }

    #[test]
    fn custom_command_with_args_stays_active() {
        let p = synced_with_custom("/project:review some args", sample_custom());
        assert!(p.is_open());
    }

    #[test]
    fn custom_command_without_args_hides_on_space() {
        let p = synced_with_custom("/user:fix ", sample_custom());
        assert!(!p.is_open());
    }

    #[test_case("/cd", "/cd", ""              ; "no_args")]
    #[test_case("/cd ~/foo", "/cd", "~/foo"   ; "with_args")]
    #[test_case("/CD ~/foo", "/cd", "~/foo"   ; "case_insensitive")]
    #[test_case("/compact ", "/compact", ""   ; "whitespace_only_args_are_empty")]
    #[test_case("/cmp", "/compact", ""    ; "fuzzy-match-1")]
    #[test_case("/cpt", "/compact", ""    ; "fuzzy-match-2")]
    #[test_case("/btw hello world", "/btw", "hello world" ; "btw_multi_word")]
    fn confirm_parses_args(input: &str, expected_name: &str, expected_args: &str) {
        let mut p = CommandPalette::new(Arc::from([]), empty_snapshot(), LuaCommandReader::empty());
        p.sync(input);
        let cmd = p.confirm(input).unwrap();
        assert_eq!(cmd.name, expected_name);
        assert_eq!(cmd.args, expected_args);
    }

    #[test]
    fn confirm_custom_command() {
        let custom = sample_custom();
        let mut p = CommandPalette::new(custom, empty_snapshot(), LuaCommandReader::empty());
        p.sync("/project:review");
        assert!(p.is_open());
        let cmd = p.confirm("/project:review some-file.rs").unwrap();
        assert_eq!(cmd.name, "/project:review");
        assert_eq!(cmd.args, "some-file.rs");
    }

    #[test]
    fn find_custom_command_lookup() {
        let custom = sample_custom();
        let p = CommandPalette::new(custom, empty_snapshot(), LuaCommandReader::empty());
        let found = p.find_custom_command("/project:review");
        assert!(found.is_some());
        assert_eq!(found.unwrap().content, "Review $ARGUMENTS");
        assert!(p.find_custom_command("/nonexistent").is_none());
    }

    fn sample_prompts() -> McpSnapshotReader {
        McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: vec![],
            prompts: vec![
                McpPromptInfo {
                    display_name: "myserver:code-review".into(),
                    qualified_name: "myserver/code-review".into(),
                    description: "Review code changes".into(),
                    arguments: vec![McpPromptArg {
                        name: "diff".into(),
                        description: "The diff".into(),
                        required: true,
                    }],
                },
                McpPromptInfo {
                    display_name: "myserver:summarize".into(),
                    qualified_name: "myserver/summarize".into(),
                    description: "Summarize text".into(),
                    arguments: vec![],
                },
            ],
            pids: vec![],
            generation: 0,
        })
    }

    fn synced_with_prompts(input: &str) -> CommandPalette {
        let mut p = CommandPalette::new(Arc::from([]), sample_prompts(), LuaCommandReader::empty());
        p.sync(input);
        p
    }

    #[test]
    fn filter_mcp_prompt_by_substring() {
        let p = synced_with_prompts("/code");
        assert!(p.is_open());
        assert_eq!(p.filtered.len(), 1);
        assert!(matches!(
            p.filtered[0].command_type,
            CommandType::McpPrompt(0)
        ));
    }

    #[test]
    fn mcp_prompt_with_args_stays_active() {
        let p = synced_with_prompts("/myserver:code-review some diff");
        assert!(p.is_open());
    }

    #[test]
    fn mcp_prompt_without_args_hides_on_space() {
        let p = synced_with_prompts("/myserver:summarize ");
        assert!(
            !p.filtered
                .iter()
                .any(|f| matches!(f.command_type, CommandType::McpPrompt(1)))
        );
    }

    #[test]
    fn find_mcp_prompt_lookup() {
        let p = synced_with_prompts("/");
        let found = p.find_mcp_prompt("/myserver:code-review");
        assert!(found.is_some());
        assert_eq!(found.unwrap().qualified_name, "myserver/code-review");
        assert!(p.find_mcp_prompt("/nonexistent").is_none());
    }

    #[test]
    fn confirm_mcp_prompt_parses_args() {
        let input = "/myserver:code-review my-diff-content";
        let mut p = synced_with_prompts(input);
        p.selected = p
            .filtered
            .iter()
            .position(|f| matches!(f.command_type, CommandType::McpPrompt(0)))
            .unwrap();
        let cmd = p.confirm(input).unwrap();
        assert_eq!(cmd.name, "/myserver:code-review");
        assert_eq!(cmd.args, "my-diff-content");
    }

    #[test]
    fn mcp_update_clears_old_prompts() {
        let reader = sample_prompts();
        let mut p = CommandPalette::new(Arc::from([]), reader, LuaCommandReader::empty());

        p.sync("/");
        let initial_count = p
            .filtered
            .iter()
            .filter(|f| matches!(f.command_type, CommandType::McpPrompt(_)))
            .count();
        assert_eq!(initial_count, 2, "Should have 2 MCP prompts initially");

        let updated_reader = McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: vec![],
            prompts: vec![McpPromptInfo {
                display_name: "myserver:new-prompt".into(),
                qualified_name: "myserver/new-prompt".into(),
                description: "A new prompt".into(),
                arguments: vec![],
            }],
            pids: vec![],
            generation: 1,
        });

        p.mcp_reader = updated_reader;
        p.sync("/");

        let updated_count = p
            .filtered
            .iter()
            .filter(|f| matches!(f.command_type, CommandType::McpPrompt(_)))
            .count();
        assert_eq!(
            updated_count, 1,
            "Should have only 1 MCP prompt after update"
        );

        assert!(!p.filtered.is_empty(), "Should have filtered results");
        let prompt = &p
            .filtered
            .iter()
            .find(|f| matches!(f.command_type, CommandType::McpPrompt(_)))
            .expect("Should have at least one MCP prompt");
        match &prompt.command_type {
            CommandType::McpPrompt(i) => {
                assert_eq!(p.mcp_prompts[*i].display_name, "myserver:new-prompt");
            }
            _ => panic!("Should have MCP prompt"),
        }
    }

    #[test_case("/cmp", "/compact" ; "compact_fuzzy")]
    #[test_case("/new", "/new" ; "new_exact")]
    #[test_case("/thm", "/theme" ; "theme_fuzzy")]
    fn nucleo_highlights_matching_indices(input: &str, expected_cmd: &str) {
        let p = synced(input);
        assert!(p.is_open(), "Input '{}' should activate palette", input);
        // Find the expected match
        let matched = p
            .filtered
            .iter()
            .find(|m| p.item_name(m) == expected_cmd)
            .unwrap_or_else(|| panic!("Should find {} for input {}", expected_cmd, input));
        // Should have some highlight indices
        assert!(
            !matched.indices.is_empty(),
            "Match should have highlight indices"
        );
    }

    fn sample_lua_commands() -> LuaCommandReader {
        LuaCommandReader::from_commands(vec![
            LuaCommandInfo {
                name: Arc::from("/memory"),
                description: Arc::from("View memory files"),
                plugin: Arc::from("memory"),
                reach: PluginReach::new(PluginPermissions::trusted()),
                max_args: 0,
            },
            LuaCommandInfo {
                name: Arc::from("/deploy"),
                description: Arc::from("Deploy the project"),
                plugin: Arc::from("deploy_plugin"),
                reach: PluginReach::new(PluginPermissions::trusted()),
                max_args: 0,
            },
        ])
    }

    fn synced_with_nargs(input: &str, max_args: usize) -> CommandPalette {
        let reader = LuaCommandReader::from_commands(vec![LuaCommandInfo {
            name: Arc::from("/rename"),
            description: Arc::from("Rename the current session"),
            plugin: Arc::from("sessions"),
            reach: PluginReach::new(PluginPermissions::trusted()),
            max_args,
        }]);
        let mut p = CommandPalette::new(Arc::from([]), empty_snapshot(), reader);
        p.sync(input);
        p
    }

    #[test_case("/rename", usize::MAX, true           ; "nargs_plus_no_args")]
    #[test_case("/rename ", usize::MAX, true          ; "nargs_plus_trailing_space")]
    #[test_case("/rename my title", usize::MAX, true  ; "nargs_plus_multi_word")]
    #[test_case("/rename title", 1, true              ; "nargs_one_single_word")]
    #[test_case("/rename my title", 1, false          ; "nargs_one_too_many")]
    #[test_case("/rename", 0, true                    ; "nargs_zero_no_args")]
    #[test_case("/rename title", 0, false             ; "nargs_zero_with_arg")]
    fn lua_command_respects_nargs(input: &str, max_args: usize, expect_active: bool) {
        assert_eq!(synced_with_nargs(input, max_args).is_open(), expect_active);
    }

    #[test]
    fn confirm_lua_command_keeps_multi_word_args() {
        let input = "/rename my new title";
        let cmd = synced_with_nargs(input, usize::MAX).confirm(input).unwrap();
        assert_eq!(cmd.name, "/rename");
        assert_eq!(cmd.args, "my new title");
    }

    fn synced_with_lua(input: &str) -> CommandPalette {
        let mut p = CommandPalette::new(Arc::from([]), empty_snapshot(), sample_lua_commands());
        p.sync(input);
        p
    }

    #[test]
    fn lua_commands_appear_in_unfiltered_list() {
        let p = synced_with_lua("/");
        let lua_count = p
            .filtered
            .iter()
            .filter(|f| matches!(f.command_type, CommandType::Lua(_)))
            .count();
        assert_eq!(lua_count, 2);
    }

    #[test]
    fn lua_command_filtered_by_substring() {
        let p = synced_with_lua("/mem");
        assert!(p.is_open());
        let found = p
            .filtered
            .iter()
            .any(|f| matches!(f.command_type, CommandType::Lua(_)) && p.item_name(f) == "/memory");
        assert!(found);
    }

    #[test]
    fn find_lua_command_returns_matching_entry() {
        let p = synced_with_lua("/");
        let found = p.find_lua_command("/memory");
        assert!(found.is_some());
        assert_eq!(found.unwrap().plugin.as_ref(), "memory");
        assert!(p.find_lua_command("/nonexistent").is_none());
    }

    #[test]
    fn confirm_lua_command_parses_args() {
        let mut p = CommandPalette::new(Arc::from([]), empty_snapshot(), sample_lua_commands());
        p.sync("/memory");
        let cmd = p.confirm("/memory some-arg").unwrap();
        assert_eq!(cmd.name, "/memory");
        assert_eq!(cmd.args, "some-arg");
    }

    #[test]
    fn packupdate_bang_reaches_the_builtin_handler() {
        let input = "/packupdate! ++lockfile demo";
        let command = synced(input).confirm(input).unwrap();
        assert_eq!(command.name, "/packupdate");
        assert_eq!(command.args, "++lockfile demo");
        assert!(command.bang);
    }

    #[test]
    fn exact_match_ranks_first_without_hiding_fuzzy_matches() {
        let reader = LuaCommandReader::from_commands(vec![LuaCommandInfo {
            name: Arc::from("/models"),
            description: Arc::from("List models"),
            plugin: Arc::from("models"),
            reach: PluginReach::new(PluginPermissions::trusted()),
            max_args: 0,
        }]);
        let mut palette = CommandPalette::new(Arc::from([]), empty_snapshot(), reader);

        palette.sync("/model");

        assert_eq!(palette.item_name(&palette.filtered[0]), "/model");
        assert!(
            palette
                .filtered
                .iter()
                .any(|item| palette.item_name(item) == "/models")
        );
        palette.move_down();
        assert_eq!(palette.confirm("/model").unwrap().name, "/models");
    }

    /// A full name the input rules out empties the palette instead of leaving
    /// its fuzzy neighbors, which Enter would then run.
    #[test_case("/packdel! demo", true ; "bang_on_a_command_that_reads_one")]
    #[test_case("/reload!", false      ; "bang_on_a_command_that_ignores_it")]
    #[test_case("/cd one two", false   ; "more_args_than_the_command_takes")]
    fn bang_and_arg_count_decide_what_the_palette_offers(input: &str, active: bool) {
        assert_eq!(synced(input).is_open(), active);
    }

    #[test]
    fn lua_commands_update_on_generation_change() {
        let (writer, reader) = maki_lua::test_support::lua_command_writer_pair();
        writer.publish(vec![LuaCommandInfo {
            name: Arc::from("/old"),
            description: Arc::from("old command"),
            plugin: Arc::from("p"),
            reach: PluginReach::new(PluginPermissions::trusted()),
            max_args: 0,
        }]);
        let mut p = CommandPalette::new(Arc::from([]), empty_snapshot(), reader);
        p.sync("/");
        let initial_lua = p
            .filtered
            .iter()
            .filter(|f| matches!(f.command_type, CommandType::Lua(_)))
            .count();
        assert_eq!(initial_lua, 1);

        writer.publish(vec![
            LuaCommandInfo {
                name: Arc::from("/new1"),
                description: Arc::from("new"),
                plugin: Arc::from("p"),
                reach: PluginReach::new(PluginPermissions::trusted()),
                max_args: 0,
            },
            LuaCommandInfo {
                name: Arc::from("/new2"),
                description: Arc::from("new2"),
                plugin: Arc::from("p"),
                reach: PluginReach::new(PluginPermissions::trusted()),
                max_args: 0,
            },
        ]);
        p.sync("/");
        let updated_lua = p
            .filtered
            .iter()
            .filter(|f| matches!(f.command_type, CommandType::Lua(_)))
            .count();
        assert_eq!(updated_lua, 2);
        assert!(p.find_lua_command("/old").is_none());
        assert!(p.find_lua_command("/new1").is_some());
    }

    #[test]
    fn reads_follow_the_buffer_and_esc_holds_until_it_changes() {
        let mut p = CommandPalette::new(Arc::from([]), empty_snapshot(), LuaCommandReader::empty());
        let mut input = TextBuffer::new("/he".into());
        assert!(p.is_active(&input));

        p.handle_key(key(KeyCode::Esc), &input);
        assert!(!p.is_active(&input));

        input.set_value("/hel");
        assert!(p.is_active(&input));

        input.set_value("hello");
        assert!(!p.is_active(&input));
    }

    fn many_custom(n: usize) -> Arc<[CustomCommand]> {
        Arc::from(
            (0..n)
                .map(|i| CustomCommand {
                    name: format!("cmd{i:02}"),
                    description: "d".into(),
                    content: String::new(),
                    scope: maki_agent::command::CommandScope::User,
                    accepts_args: false,
                })
                .collect::<Vec<_>>(),
        )
    }

    fn render_rows(p: &mut CommandPalette, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let input_area = Rect::new(0, height - 1, width, 1);
        terminal
            .draw(|frame| {
                p.render(frame, input_area);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let area = buffer.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn view_scrolls_to_keep_the_selection_on_screen() {
        let mut p = synced_with_custom("/", many_custom(30));
        let last = p.filtered.len() - 1;
        p.selected = last;
        let last_name = p.item_name(&p.filtered[last]).to_string();
        let first_name = p.item_name(&p.filtered[0]).to_string();

        let text = render_rows(&mut p, 60, 10);
        assert!(
            text.contains(&last_name),
            "selected item off screen:\n{text}"
        );
        assert!(
            !text.contains(&first_name),
            "window did not scroll:\n{text}"
        );

        p.selected = 0;
        let text = render_rows(&mut p, 60, 10);
        assert!(
            text.contains(&first_name),
            "scroll did not return to top:\n{text}"
        );
    }

    #[test]
    fn view_keeps_the_window_when_selection_is_already_visible() {
        let mut p = synced_with_custom("/", many_custom(30));
        p.selected = 5;
        let first_name = p.item_name(&p.filtered[0]).to_string();
        let text = render_rows(&mut p, 60, 10);
        assert!(
            text.contains(&first_name),
            "window scrolled for no reason:\n{text}"
        );
    }

    #[test]
    fn view_clamps_the_window_after_the_list_shrinks() {
        let mut p = synced_with_custom("/", many_custom(30));
        p.selected = p.filtered.len() - 1;
        render_rows(&mut p, 60, 10);

        p.sync("/cmd0");
        let first_name = p.item_name(&p.filtered[0]).to_string();
        let text = render_rows(&mut p, 60, 20);
        assert!(
            text.contains(&first_name),
            "window stayed scrolled after the filter shrank the list:\n{text}"
        );
    }
}
