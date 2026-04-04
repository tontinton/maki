use std::sync::Arc;

use arc_swap::ArcSwap;
use crossterm::event::{KeyCode, KeyEvent};
use maki_agent::McpPromptInfo;
use maki_agent::command::CustomCommand;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
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
        name: "/queue",
        description: "Remove items from queue",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/sessions",
        description: "Browse and switch sessions",
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
        name: "/memory",
        description: "List memory files for this project",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/yolo",
        description: "Toggle YOLO mode (skip all permission prompts)",
        max_args: 0,
    },
    BuiltinCommand {
        name: "/thinking",
        description: "Toggle extended thinking (off, adaptive, or budget)",
        max_args: 1,
    },
    BuiltinCommand {
        name: "/exit",
        description: "Exit the application",
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
    Close,
    Passthrough,
}

#[derive(Clone)]
enum FilteredItem {
    Builtin(usize),
    Custom(usize),
    McpPrompt(usize),
}

pub struct CommandPalette {
    selected: usize,
    filtered: Vec<FilteredItem>,
    custom: Arc<[CustomCommand]>,
    mcp_prompts_source: Arc<ArcSwap<Vec<McpPromptInfo>>>,
    mcp_prompts: Arc<Vec<McpPromptInfo>>,
}

impl CommandPalette {
    pub fn new(
        custom_commands: Arc<[CustomCommand]>,
        mcp_prompts: Arc<ArcSwap<Vec<McpPromptInfo>>>,
    ) -> Self {
        let snapshot = mcp_prompts.load_full();
        Self {
            selected: 0,
            filtered: Vec::new(),
            custom: custom_commands,
            mcp_prompts_source: mcp_prompts,
            mcp_prompts: snapshot,
        }
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
                self.close();
                CommandAction::Close
            }
            _ => CommandAction::Passthrough,
        }
    }

    pub fn is_active(&self) -> bool {
        !self.filtered.is_empty()
    }

    pub fn sync(&mut self, input: &str) {
        self.mcp_prompts = self.mcp_prompts_source.load_full();
        let Some(stripped) = input.strip_prefix('/') else {
            self.filtered.clear();
            return;
        };
        let parts: Vec<&str> = stripped.split_whitespace().collect();
        let cmd_word = parts.first().copied().unwrap_or(stripped);
        let cmd_lower = cmd_word.to_ascii_lowercase();
        let trailing_space = stripped.ends_with(char::is_whitespace);
        let arg_count = if trailing_space {
            parts.len()
        } else {
            parts.len().saturating_sub(1)
        };

        self.filtered.clear();

        for (i, cmd) in BUILTIN_COMMANDS.iter().enumerate() {
            if cmd.name[1..].to_ascii_lowercase().starts_with(&cmd_lower)
                && arg_count <= cmd.max_args
            {
                self.filtered.push(FilteredItem::Builtin(i));
            }
        }

        for (i, cmd) in self.custom.iter().enumerate() {
            let display = cmd.display_name();
            let entry_name = &display[1..];
            let max_args = if cmd.has_args() { usize::MAX } else { 0 };
            if entry_name.to_ascii_lowercase().starts_with(&cmd_lower) && arg_count <= max_args {
                self.filtered.push(FilteredItem::Custom(i));
            }
        }

        for (i, prompt) in self.mcp_prompts.iter().enumerate() {
            let max_args = if prompt.arguments.is_empty() {
                0
            } else {
                usize::MAX
            };
            if prompt
                .display_name
                .to_ascii_lowercase()
                .starts_with(&cmd_lower)
                && arg_count <= max_args
            {
                self.filtered.push(FilteredItem::McpPrompt(i));
            }
        }

        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    pub fn close(&mut self) {
        self.filtered.clear();
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

    fn item_name(&self, item: &FilteredItem) -> String {
        match item {
            FilteredItem::Builtin(i) => BUILTIN_COMMANDS[*i].name.to_string(),
            FilteredItem::Custom(i) => self.custom[*i].display_name(),
            FilteredItem::McpPrompt(i) => format!("/{}", self.mcp_prompts[*i].display_name),
        }
    }

    fn item_description(&self, item: &FilteredItem) -> &str {
        match item {
            FilteredItem::Builtin(i) => BUILTIN_COMMANDS[*i].description,
            FilteredItem::Custom(i) => &self.custom[*i].description,
            FilteredItem::McpPrompt(i) => &self.mcp_prompts[*i].description,
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

        let names: Vec<String> = filtered.iter().map(|item| self.item_name(item)).collect();

        let lines: Vec<Line> = filtered
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let name = &names[i];
                let desc = self.item_description(item);
                let selected = i == self.selected;
                let name_pad = max_name - name.len() + GAP;
                if selected {
                    let s = theme::current().cmd_selected;
                    Line::from(vec![
                        Span::styled(" ".repeat(PAD), s),
                        Span::styled(name.clone(), s),
                        Span::styled(" ".repeat(name_pad), s),
                        Span::styled(desc, s),
                        Span::styled(" ".repeat(PAD), s),
                    ])
                } else {
                    Line::from(vec![
                        Span::raw(" ".repeat(PAD)),
                        Span::styled(name.clone(), theme::current().cmd_name),
                        Span::raw(" ".repeat(name_pad)),
                        Span::styled(desc, theme::current().cmd_desc),
                        Span::raw(" ".repeat(PAD)),
                    ])
                }
            })
            .collect();

        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).style(Style::new().bg(theme::current().background)),
            popup,
        );

        Some(popup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::McpPromptArg;
    use test_case::test_case;

    fn empty_prompts() -> Arc<ArcSwap<Vec<McpPromptInfo>>> {
        Arc::new(ArcSwap::from_pointee(Vec::new()))
    }

    fn synced(input: &str) -> CommandPalette {
        let mut p = CommandPalette::new(Arc::from([]), empty_prompts());
        p.sync(input);
        p
    }

    fn synced_with_custom(input: &str, custom: Arc<[CustomCommand]>) -> CommandPalette {
        let mut p = CommandPalette::new(custom, empty_prompts());
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

    #[test_case("/co", true ; "compact_prefix")]
    #[test_case("/ne", true ; "lowercase_prefix")]
    #[test_case("/NE", true ; "uppercase_prefix")]
    #[test_case("/zzz", false ; "no_match")]
    fn filter_by_prefix(input: &str, expect_active: bool) {
        let p = synced(input);
        assert_eq!(p.is_active(), expect_active);
    }

    #[test]
    fn filter_custom_by_prefix() {
        let p = synced_with_custom("/project:r", sample_custom());
        assert!(p.is_active());
        assert_eq!(p.filtered.len(), 1);
        assert!(matches!(p.filtered[0], FilteredItem::Custom(0)));
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
        let p = CommandPalette::new(Arc::from([]), empty_prompts());
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
    #[test_case("/btw hello world", "/btw", "hello world" ; "btw_multi_word")]
    fn confirm_parses_args(input: &str, expected_name: &str, expected_args: &str) {
        let mut p = CommandPalette::new(Arc::from([]), empty_prompts());
        p.sync(input);
        let cmd = p.confirm(input).unwrap();
        assert_eq!(cmd.name, expected_name);
        assert_eq!(cmd.args, expected_args);
    }

    #[test]
    fn confirm_custom_command() {
        let custom = sample_custom();
        let mut p = CommandPalette::new(custom, empty_prompts());
        p.sync("/project:review");
        assert!(p.is_active());
        let cmd = p.confirm("/project:review some-file.rs").unwrap();
        assert_eq!(cmd.name, "/project:review");
        assert_eq!(cmd.args, "some-file.rs");
    }

    #[test]
    fn find_custom_command_lookup() {
        let custom = sample_custom();
        let p = CommandPalette::new(custom, empty_prompts());
        let found = p.find_custom_command("/project:review");
        assert!(found.is_some());
        assert_eq!(found.unwrap().content, "Review $ARGUMENTS");
        assert!(p.find_custom_command("/nonexistent").is_none());
    }

    fn sample_prompts() -> Arc<ArcSwap<Vec<McpPromptInfo>>> {
        Arc::new(ArcSwap::from_pointee(vec![
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
        ]))
    }

    fn synced_with_prompts(input: &str) -> CommandPalette {
        let mut p = CommandPalette::new(Arc::from([]), sample_prompts());
        p.sync(input);
        p
    }

    #[test]
    fn filter_mcp_prompt_by_prefix() {
        let p = synced_with_prompts("/myserver:c");
        assert!(p.is_active());
        assert_eq!(p.filtered.len(), 1);
        assert!(matches!(p.filtered[0], FilteredItem::McpPrompt(0)));
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
                .any(|f| matches!(f, FilteredItem::McpPrompt(1)))
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
            .position(|f| matches!(f, FilteredItem::McpPrompt(0)))
            .unwrap();
        let cmd = p.confirm(input).unwrap();
        assert_eq!(cmd.name, "/myserver:code-review");
        assert_eq!(cmd.args, "my-diff-content");
    }
}
