use crate::theme;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

struct Command {
    name: &'static str,
    description: &'static str,
    max_args: usize,
}

const COMMANDS: &[Command] = &[
    Command {
        name: "/tasks",
        description: "Browse and search tasks",
        max_args: 0,
    },
    Command {
        name: "/compact",
        description: "Summarize and compact conversation history",
        max_args: 0,
    },
    Command {
        name: "/new",
        description: "Start a new session",
        max_args: 0,
    },
    Command {
        name: "/help",
        description: "Show keybindings",
        max_args: 0,
    },
    Command {
        name: "/queue",
        description: "Remove items from queue",
        max_args: 0,
    },
    Command {
        name: "/sessions",
        description: "Browse and switch sessions",
        max_args: 0,
    },
    Command {
        name: "/model",
        description: "Switch model",
        max_args: 0,
    },
    Command {
        name: "/theme",
        description: "Switch color theme",
        max_args: 0,
    },
    Command {
        name: "/mcp",
        description: "Configure MCP servers",
        max_args: 0,
    },
    Command {
        name: "/cd",
        description: "Change working directory",
        max_args: 1,
    },
    Command {
        name: "/btw",
        description: "Ask a quick question (no tools, no history pollution)",
        max_args: usize::MAX,
    },
    Command {
        name: "/memory",
        description: "List memory files for this project",
        max_args: 0,
    },
    Command {
        name: "/yolo",
        description: "Toggle YOLO mode (skip all permission prompts)",
        max_args: 0,
    },
    Command {
        name: "/exit",
        description: "Exit the application",
        max_args: 0,
    },
];

pub struct ParsedCommand {
    pub name: &'static str,
    pub args: String,
}

#[cfg(test)]
fn parse_command(input: &str) -> Option<ParsedCommand> {
    let stripped = input.strip_prefix('/')?;
    let (cmd_word, args) = match stripped.split_once(char::is_whitespace) {
        Some((c, a)) => (c, a.trim()),
        None => (stripped, ""),
    };
    let cmd = COMMANDS
        .iter()
        .find(|c| c.name[1..].eq_ignore_ascii_case(cmd_word))?;
    Some(ParsedCommand {
        name: cmd.name,
        args: args.to_string(),
    })
}

pub enum CommandAction {
    Consumed,
    Execute(ParsedCommand),
    Close,
    Passthrough,
}

pub struct CommandPalette {
    selected: usize,
    filtered: Vec<usize>,
}

impl CommandPalette {
    pub fn new() -> Self {
        Self {
            selected: 0,
            filtered: Vec::new(),
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
        self.filtered = COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, cmd)| {
                cmd.name[1..].to_ascii_lowercase().starts_with(&cmd_lower)
                    && arg_count <= cmd.max_args
            })
            .map(|(i, _)| i)
            .collect();
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

    pub fn confirm(&self, input: &str) -> Option<ParsedCommand> {
        let &idx = self.filtered.get(self.selected)?;
        let name = COMMANDS[idx].name;
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
            .map(|&i| COMMANDS[i].name.len())
            .max()
            .unwrap_or(0);
        let max_desc = filtered
            .iter()
            .map(|&i| COMMANDS[i].description.len())
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

        let lines: Vec<Line> = filtered
            .iter()
            .enumerate()
            .map(|(i, &cmd_idx)| {
                let cmd = &COMMANDS[cmd_idx];
                let selected = i == self.selected;
                let name_pad = max_name - cmd.name.len() + GAP;
                if selected {
                    let s = theme::current().cmd_selected;
                    Line::from(vec![
                        Span::styled(" ".repeat(PAD), s),
                        Span::styled(cmd.name, s),
                        Span::styled(" ".repeat(name_pad), s),
                        Span::styled(cmd.description, s),
                        Span::styled(" ".repeat(PAD), s),
                    ])
                } else {
                    Line::from(vec![
                        Span::raw(" ".repeat(PAD)),
                        Span::styled(cmd.name, theme::current().cmd_name),
                        Span::raw(" ".repeat(name_pad)),
                        Span::styled(cmd.description, theme::current().cmd_desc),
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
    use test_case::test_case;

    fn synced(input: &str) -> CommandPalette {
        let mut p = CommandPalette::new();
        p.sync(input);
        p
    }

    #[test]
    fn slash_shows_all_commands() {
        let p = synced("/");
        assert!(p.is_active());
        assert_eq!(p.filtered.len(), COMMANDS.len());
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
    fn navigation_wraps() {
        let mut p = synced("/");
        p.move_up();
        assert_eq!(p.selected, p.filtered.len() - 1);
        p.move_down();
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn confirm_when_inactive_returns_none() {
        let p = CommandPalette::new();
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
        assert_eq!(COMMANDS[p.filtered[0]].name, "/cd");
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

    #[test_case("/cd", Some("/cd"), ""              ; "no_args")]
    #[test_case("/cd ~/foo", Some("/cd"), "~/foo"   ; "with_args")]
    #[test_case("/CD ~/foo", Some("/cd"), "~/foo"   ; "case_insensitive")]
    #[test_case("/compact", Some("/compact"), ""    ; "other_command")]
    #[test_case("/btw hello world", Some("/btw"), "hello world" ; "btw_multi_word")]
    #[test_case("/nonexistent", None, ""            ; "unknown")]
    #[test_case("hello", None, ""                   ; "no_slash")]
    fn parse_command_cases(input: &str, expected_name: Option<&str>, expected_args: &str) {
        let result = parse_command(input);
        match expected_name {
            Some(name) => {
                let cmd = result.unwrap();
                assert_eq!(cmd.name, name);
                assert_eq!(cmd.args, expected_args);
            }
            None => assert!(result.is_none()),
        }
    }
}
