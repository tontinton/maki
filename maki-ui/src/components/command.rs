use std::mem;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use maki_agent::command::CustomCommand;
use maki_agent::{McpPromptInfo, McpSnapshotReader};
use maki_lua::{LuaCommandInfo, LuaCommandReader};
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Matcher, Nucleo, Utf32String};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::theme;

pub struct BuiltinCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub max_args: usize,
}

pub const BUILTIN_COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand {
        name: "/tasks",
        description: "Browse and search tasks",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/compact",
        description: "Summarize and compact conversation history",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/new",
        description: "Start a new session",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/help",
        description: "Show keybindings",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/usage",
        description: "Show token usage breakdown",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/queue",
        description: "Remove items from queue",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/model",
        description: "Switch model",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/theme",
        description: "Switch color theme",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/mcp",
        description: "Configure MCP servers",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/login",
        description: "Authenticate with an LLM provider",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/cd",
        description: "Change working directory",
        max_args: 1,
    },
    BuiltinCommand {
        name: "/btw",
        description: "Ask a quick question (no tools, no history pollution)",
        max_args: usize::MAX,
    },
    BuiltinCommand {
        name: "/yolo",
        description: "Toggle YOLO mode (skip all permission prompts)",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/thinking",
        description: "Toggle extended thinking (off, adaptive, effort level, or budget)",
        max_args: 1,
    },
    BuiltinCommand {
        name: "/fast",
        description: "Toggle Anthropic fast mode (Opus only)",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/workflow",
        description: "Toggle workflow mode (task callable inside code_execution)",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/exit",
        description: "Exit the application",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/reload",
        description: "Reload plugins and config",
        max_args: 0,
    },
];

pub struct ParsedCommand {
    pub name: String,
    pub args: String,
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

pub struct CommandPalette {
    selected: usize,
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
            selected: 0,
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
        }
    }

    fn build_nucleo(
        custom_commands: &[CustomCommand],
        mcp_prompts: &[McpPromptInfo],
        lua_commands: &[LuaCommandInfo],
    ) -> Nucleo<CommandItem> {
        let nucleo = Nucleo::new(Config::DEFAULT, Arc::new(|| {}), None, 1);
        let injector = nucleo.injector();

        for cmd in BUILTIN_COMMANDS.iter() {
            let item = CommandItem {
                name: cmd.name.to_string(),
                max_args: cmd.max_args,
                command_type: CommandType::Builtin(cmd),
            };
            injector.push(item, |item, cols| {
                cols[0] = Utf32String::from(item.name.as_str());
            });
        }

        for (i, cmd) in custom_commands.iter().enumerate() {
            let item = CommandItem {
                name: cmd.display_name(),
                max_args: if cmd.has_args() { usize::MAX } else { 0 },
                command_type: CommandType::Custom(i),
            };
            injector.push(item, |item, cols| {
                cols[0] = Utf32String::from(item.name.as_str());
            });
        }

        for (i, prompt) in mcp_prompts.iter().enumerate() {
            let item = CommandItem {
                name: format!("/{}", prompt.display_name),
                max_args: if prompt.arguments.is_empty() {
                    0
                } else {
                    usize::MAX
                },
                command_type: CommandType::McpPrompt(i),
            };
            injector.push(item, |item, cols| {
                cols[0] = Utf32String::from(item.name.as_str());
            });
        }

        for (i, cmd) in lua_commands.iter().enumerate() {
            let item = CommandItem {
                name: cmd.name.to_string(),
                max_args: 0,
                command_type: CommandType::Lua(i),
            };
            injector.push(item, |item, cols| {
                cols[0] = Utf32String::from(item.name.as_str());
            });
        }

        nucleo
    }

    pub fn handle_key(&mut self, key: KeyEvent, input: &str) -> CommandAction {
        if !self.is_active() {
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
            KeyCode::Enter => match self.confirm(input) {
                Some(cmd) => {
                    self.close();
                    CommandAction::Execute(cmd)
                }
                None => CommandAction::Consumed,
            },
            KeyCode::Tab => {
                if let Some(item) = self.filtered.get(self.selected) {
                    let name = self.item_name(item);
                    let text = if self.item_has_args(item) {
                        format!("{name} ")
                    } else {
                        name
                    };
                    CommandAction::Complete(text)
                } else {
                    CommandAction::Consumed
                }
            }
            _ => CommandAction::Passthrough,
        }
    }

    pub fn is_active(&self) -> bool {
        !self.filtered.is_empty()
    }

    pub fn sync(&mut self, input: &str) {
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
            return;
        };

        let parts: Vec<&str> = stripped.split_whitespace().collect();
        let cmd_word = parts.first().copied().unwrap_or(stripped);
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

        // Tick to get matches
        self.tick();
    }

    fn tick(&mut self) {
        let status = self.nucleo.tick(100);
        if status.changed {
            self.refresh_matches();
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
        self.current_arg_count = 0;
    }

    pub fn move_up(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.filtered.len() - 1
        } else {
            self.selected - 1
        };
    }

    pub fn move_down(&mut self) {
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
            CommandType::Lua(_) => false,
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

    pub fn confirm(&self, input: &str) -> Option<ParsedCommand> {
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
        })
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

    pub fn view(&self, frame: &mut Frame, input_area: Rect) -> Option<Rect> {
        let filtered = &self.filtered;
        if filtered.is_empty() {
            return None;
        }

        let popup_height = (filtered.len() as u16).min(input_area.y);
        if popup_height == 0 {
            return None;
        }

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
        let lines: Vec<Line> = filtered
            .iter()
            .enumerate()
            .map(|(i, m)| {
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
    use maki_agent::{McpPromptArg, McpSnapshot};
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
        assert!(!p.is_active());
    }

    #[test_case("/mp", true ; "compact_substring")]
    #[test_case("/ew", true ; "lowercase_substring")]
    #[test_case("/EW", true ; "uppercase_substring")]
    #[test_case("/zzz", false ; "no_match")]
    fn filter_by_substring(input: &str, expect_active: bool) {
        let p = synced(input);
        assert_eq!(p.is_active(), expect_active);
    }

    #[test]
    fn filter_custom_by_substring() {
        let p = synced_with_custom("/review", sample_custom());
        assert!(p.is_active());
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
        assert!(p.is_active());
        assert_eq!(p.filtered.len(), 1);
        let name = p.item_name(&p.filtered[0]);
        assert_eq!(name, "/cd");
    }

    #[test_case("/compact ", false ; "zero_arg_cmd_with_space")]
    #[test_case("/tasks ", false   ; "zero_arg_tasks_with_space")]
    #[test_case("/cd ", true        ; "one_arg_cmd_with_space")]
    #[test_case("/cd ~/foo", true   ; "one_arg_cmd_mid_arg")]
    #[test_case("/cd  ~/foo", true  ; "one_arg_cmd_double_space")]
    #[test_case("/cd ~/foo ", false ; "one_arg_cmd_second_space")]
    #[test_case("/btw hello world", true ; "btw_stays_active_with_many_args")]
    fn sync_respects_max_args(input: &str, expect_active: bool) {
        let p = synced(input);
        assert_eq!(p.is_active(), expect_active);
    }

    #[test]
    fn custom_command_with_args_stays_active() {
        let p = synced_with_custom("/project:review some args", sample_custom());
        assert!(p.is_active());
    }

    #[test]
    fn custom_command_without_args_hides_on_space() {
        let p = synced_with_custom("/user:fix ", sample_custom());
        assert!(!p.is_active());
    }

    #[test_case("/cd", "/cd", ""              ; "no_args")]
    #[test_case("/cd ~/foo", "/cd", "~/foo"   ; "with_args")]
    #[test_case("/CD ~/foo", "/cd", "~/foo"   ; "case_insensitive")]
    #[test_case("/compact", "/compact", ""    ; "other_command")]
    #[test_case("/cmp", "/compact", ""    ; "fuzzy-match-1")]
    #[test_case("/pct", "/compact", ""    ; "fuzzy-match-2")]
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
        assert!(p.is_active());
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
        assert!(p.is_active());
        assert_eq!(p.filtered.len(), 1);
        assert!(matches!(
            p.filtered[0].command_type,
            CommandType::McpPrompt(0)
        ));
    }

    #[test]
    fn mcp_prompt_with_args_stays_active() {
        let p = synced_with_prompts("/myserver:code-review some diff");
        assert!(p.is_active());
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
    #[test_case("/tsk", "/tasks" ; "tasks_fuzzy")]
    fn nucleo_highlights_matching_indices(input: &str, expected_cmd: &str) {
        let p = synced(input);
        assert!(p.is_active(), "Input '{}' should activate palette", input);
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
            },
            LuaCommandInfo {
                name: Arc::from("/deploy"),
                description: Arc::from("Deploy the project"),
                plugin: Arc::from("deploy_plugin"),
            },
        ])
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
        assert!(p.is_active());
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
    fn lua_commands_update_on_generation_change() {
        let (writer, reader) = maki_lua::test_support::lua_command_writer_pair();
        writer.publish(vec![LuaCommandInfo {
            name: Arc::from("/old"),
            description: Arc::from("old command"),
            plugin: Arc::from("p"),
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
            },
            LuaCommandInfo {
                name: Arc::from("/new2"),
                description: Arc::from("new2"),
                plugin: Arc::from("p"),
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
}
