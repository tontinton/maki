use crate::components::keybindings::key;
use crate::theme;
use maki_agent::{TodoItem, TodoStatus, ToolOutput};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use std::collections::HashMap;

#[allow(dead_code)]
const PANEL_TITLE: &str = " Todos ";
#[allow(dead_code)]
const HIDE_DESC: &str = " to hide ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Shown,
    Hidden,
    UserDismissed,
}

impl Visibility {
    fn is_shown(self) -> bool {
        matches!(self, Self::Shown)
    }
}

pub struct TodoPanel {
    visibility: Visibility,
    items: Vec<TodoItem>,
}

impl TodoPanel {
    pub fn new() -> Self {
        Self {
            visibility: Visibility::Hidden,
            items: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.visibility = Visibility::Hidden;
        self.items.clear();
    }

    pub fn on_todowrite(&mut self, items: &[TodoItem]) {
        self.items = items.to_vec();
        if items.is_empty() {
            self.visibility = Visibility::Hidden;
        } else if self.visibility != Visibility::UserDismissed {
            self.visibility = Visibility::Shown;
        }
    }

    pub fn restore(&mut self, tool_outputs: &HashMap<String, ToolOutput>, dismissed: bool) {
        self.items = extract_last_todos(tool_outputs);
        self.visibility = if self.items.is_empty() {
            Visibility::Hidden
        } else if dismissed {
            Visibility::UserDismissed
        } else {
            Visibility::Shown
        };
    }

    pub fn is_user_dismissed(&self) -> bool {
        self.visibility == Visibility::UserDismissed
    }

    pub fn toggle(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.visibility = if self.visibility.is_shown() {
            Visibility::UserDismissed
        } else {
            Visibility::Shown
        };
    }

    #[allow(dead_code)]
    pub fn on_turn_done(&mut self) {
        self.reset();
    }

    #[allow(dead_code)]
    pub fn height(&self) -> u16 {
        if !self.visibility.is_shown() || self.items.is_empty() {
            0
        } else {
            self.items.len() as u16 + 2
        }
    }

    #[allow(dead_code)]
    pub fn view(&self, frame: &mut Frame, area: Rect) {
        if self.items.is_empty() {
            return;
        }

        let t = theme::current();
        let lines: Vec<Line> = self
            .items
            .iter()
            .map(|item| {
                let style = match item.status {
                    TodoStatus::Completed => t.todo_completed,
                    TodoStatus::InProgress => t.todo_in_progress,
                    TodoStatus::Pending => t.todo_pending,
                    TodoStatus::Cancelled => t.todo_cancelled,
                };
                Line::from(Span::styled(
                    format!("{} {}", item.status.marker(), item.content),
                    style,
                ))
            })
            .collect();

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(t.panel_border)
            .title_top(Line::from(PANEL_TITLE).left_aligned())
            .title_bottom(
                Line::from(vec![
                    Span::styled(format!(" {}", key::TODO_PANEL.label), t.keybind_key),
                    Span::styled(HIDE_DESC, t.tool_dim),
                ])
                .right_aligned(),
            )
            .title_style(t.panel_title);

        let paragraph = Paragraph::new(lines)
            .style(Style::new().fg(t.foreground))
            .block(block);

        frame.render_widget(paragraph, area);
    }
}

fn extract_last_todos(outputs: &HashMap<String, ToolOutput>) -> Vec<TodoItem> {
    outputs
        .values()
        .find_map(|o| match o {
            ToolOutput::TodoList(items) => Some(items.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::{TodoPriority, TodoStatus};

    fn make_items(n: usize) -> Vec<TodoItem> {
        (0..n)
            .map(|i| TodoItem {
                content: format!("task {i}"),
                status: TodoStatus::Pending,
                priority: TodoPriority::Medium,
            })
            .collect()
    }

    #[test]
    fn on_todowrite_lifecycle() {
        let mut panel = TodoPanel::new();

        panel.on_todowrite(&make_items(2));
        assert_eq!(panel.visibility, Visibility::Shown);
        assert_eq!(panel.items.len(), 2);

        panel.on_todowrite(&make_items(1));
        assert_eq!(panel.items.len(), 1);

        panel.on_todowrite(&[]);
        assert_eq!(panel.visibility, Visibility::Hidden);
        assert!(panel.items.is_empty());
    }

    #[test]
    fn user_dismiss_survives_todowrite_but_not_turn_end() {
        let mut panel = TodoPanel::new();

        panel.on_todowrite(&make_items(2));
        panel.toggle();
        assert_eq!(panel.visibility, Visibility::UserDismissed);

        panel.on_todowrite(&make_items(3));
        assert_eq!(panel.visibility, Visibility::UserDismissed);

        panel.on_turn_done();
        assert_eq!(panel.visibility, Visibility::Hidden);

        panel.on_todowrite(&make_items(1));
        assert_eq!(panel.visibility, Visibility::Shown);
    }

    #[test]
    fn height_scales_with_items() {
        let mut panel = TodoPanel::new();
        assert_eq!(panel.height(), 0);

        panel.on_todowrite(&make_items(3));
        assert_eq!(panel.height(), 5);

        panel.toggle();
        assert_eq!(panel.height(), 0);
    }

    #[test]
    fn toggle_noop_when_empty() {
        let mut panel = TodoPanel::new();
        panel.toggle();
        assert_eq!(panel.visibility, Visibility::Hidden);
    }

    #[test]
    fn restore_shows_panel_and_resets_dismiss() {
        let mut panel = TodoPanel::new();
        panel.on_todowrite(&make_items(2));
        panel.toggle();
        assert_eq!(panel.visibility, Visibility::UserDismissed);

        let mut outputs = HashMap::new();
        outputs.insert("id".to_string(), ToolOutput::TodoList(make_items(3)));
        panel.restore(&outputs, false);
        assert_eq!(panel.visibility, Visibility::Shown);
        assert_eq!(panel.items.len(), 3);
    }

    #[test]
    fn restore_hides_when_no_todos() {
        let mut panel = TodoPanel::new();
        panel.on_todowrite(&make_items(2));

        panel.restore(&HashMap::new(), false);
        assert_eq!(panel.visibility, Visibility::Hidden);
        assert!(panel.items.is_empty());
    }
}
