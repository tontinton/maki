use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use maki_agent::permissions::generalize_scope;

use crate::components::Overlay;
use crate::components::form::render_form;
use crate::components::hint_line;
use crate::theme;

const HINT_ALLOW_ROW: &[(&str, &str)] = &[
    ("y", "Allow"),
    ("a", "Always (project)"),
    ("A", "Always (all projects)"),
    ("s", "Session"),
];
const HINT_DENY_ROW: &[(&str, &str)] = &[
    ("n", "Deny"),
    ("d", "Deny-always (project)"),
    ("D", "Deny-always (all)"),
];

const CONFIRM_ALLOW_PROJECT_HINTS: &[(&str, &str)] =
    &[("y", "Confirm allow-always (project)"), ("any", "Cancel")];
const CONFIRM_ALLOW_ALL_HINTS: &[(&str, &str)] = &[
    ("y", "Confirm allow-always (all projects)"),
    ("any", "Cancel"),
];
const CONFIRM_SESSION_HINTS: &[(&str, &str)] =
    &[("y", "Confirm allow (session)"), ("any", "Cancel")];
const CONFIRM_DENY_PROJECT_HINTS: &[(&str, &str)] =
    &[("y", "Confirm deny-always (project)"), ("any", "Cancel")];
const CONFIRM_DENY_ALL_HINTS: &[(&str, &str)] = &[
    ("y", "Confirm deny-always (all projects)"),
    ("any", "Cancel"),
];

fn aligned_hint_rows(rows: &[&[(&str, &str)]]) -> Vec<Line<'static>> {
    let t = theme::current();
    let max_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut col_widths = vec![0usize; max_cols];
    for row in rows {
        for (i, (key, desc)) in row.iter().enumerate() {
            let cell_len = key.len() + 1 + desc.len();
            col_widths[i] = col_widths[i].max(cell_len);
        }
    }
    rows.iter()
        .map(|row| {
            let mut spans = Vec::with_capacity(row.len() * 2);
            for (i, (key, desc)) in row.iter().enumerate() {
                spans.push(Span::styled(format!("  {key}"), t.keybind_key));
                let cell_len = key.len() + 1 + desc.len();
                let pad = if i + 1 < row.len() {
                    col_widths[i].saturating_sub(cell_len)
                } else {
                    0
                };
                spans.push(Span::styled(
                    format!(" {desc}{:width$}", "", width = pad),
                    t.form_hint,
                ));
            }
            Line::from(spans)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionAction {
    AllowOnce,
    Deny,
    AllowAlwaysLocal,
    AllowAlwaysGlobal,
    DenyAlwaysLocal,
    DenyAlwaysGlobal,
    AllowSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PromptState {
    #[default]
    Normal,
    ConfirmAllowAlwaysLocal,
    ConfirmAllowAlwaysGlobal,
    ConfirmAllowSession,
    ConfirmDenyAlwaysLocal,
    ConfirmDenyAlwaysGlobal,
}

pub enum PermissionPrompt {
    Closed,
    Open {
        #[allow(dead_code)]
        id: String,
        tool: String,
        scope: String,
        subagent_id: Option<String>,
        generalized_scopes: Vec<String>,
        state: PromptState,
    },
}

impl Overlay for PermissionPrompt {
    fn is_open(&self) -> bool {
        matches!(self, Self::Open { .. })
    }

    fn is_modal(&self) -> bool {
        false
    }

    fn close(&mut self) {
        *self = Self::Closed;
    }
}

impl PermissionPrompt {
    pub fn new() -> Self {
        Self::Closed
    }

    pub fn open(&mut self, id: String, tool: String, scope: String, subagent_id: Option<String>) {
        let generalized_scopes: Vec<String> = scope
            .split(" && ")
            .map(|s| generalize_scope(&tool, s))
            .collect();
        *self = Self::Open {
            id,
            tool,
            scope,
            subagent_id,
            generalized_scopes,
            state: PromptState::Normal,
        };
    }

    pub fn subagent_id(&self) -> Option<&str> {
        match self {
            Self::Open { subagent_id, .. } => subagent_id.as_deref(),
            Self::Closed => None,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<PermissionAction> {
        let Self::Open { state, .. } = self else {
            return None;
        };
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match *state {
            PromptState::Normal => match key.code {
                KeyCode::Char('y') => Some(PermissionAction::AllowOnce),
                KeyCode::Char('n') => Some(PermissionAction::Deny),
                KeyCode::Char('a') => {
                    *state = PromptState::ConfirmAllowAlwaysLocal;
                    None
                }
                KeyCode::Char('A') => {
                    *state = PromptState::ConfirmAllowAlwaysGlobal;
                    None
                }
                KeyCode::Char('d') => {
                    *state = PromptState::ConfirmDenyAlwaysLocal;
                    None
                }
                KeyCode::Char('D') => {
                    *state = PromptState::ConfirmDenyAlwaysGlobal;
                    None
                }
                KeyCode::Char('s') => {
                    *state = PromptState::ConfirmAllowSession;
                    None
                }
                _ => None,
            },
            PromptState::ConfirmAllowAlwaysLocal => match key.code {
                KeyCode::Char('y') => Some(PermissionAction::AllowAlwaysLocal),
                _ => {
                    *state = PromptState::Normal;
                    None
                }
            },
            PromptState::ConfirmAllowAlwaysGlobal => match key.code {
                KeyCode::Char('y') => Some(PermissionAction::AllowAlwaysGlobal),
                _ => {
                    *state = PromptState::Normal;
                    None
                }
            },
            PromptState::ConfirmAllowSession => match key.code {
                KeyCode::Char('y') => Some(PermissionAction::AllowSession),
                _ => {
                    *state = PromptState::Normal;
                    None
                }
            },
            PromptState::ConfirmDenyAlwaysLocal => match key.code {
                KeyCode::Char('y') => Some(PermissionAction::DenyAlwaysLocal),
                _ => {
                    *state = PromptState::Normal;
                    None
                }
            },
            PromptState::ConfirmDenyAlwaysGlobal => match key.code {
                KeyCode::Char('y') => Some(PermissionAction::DenyAlwaysGlobal),
                _ => {
                    *state = PromptState::Normal;
                    None
                }
            },
        }
    }

    fn build_lines(&self) -> Vec<Line<'static>> {
        let Self::Open {
            tool,
            scope,
            subagent_id,
            generalized_scopes,
            state,
            ..
        } = self
        else {
            return vec![];
        };
        let t = theme::current();
        let label_style = t.tool_dim;
        let value_style = Style::new().fg(t.foreground);

        let mut tool_spans = vec![Span::raw("  "), Span::styled("tool  ", label_style)];
        if subagent_id.is_some() {
            tool_spans.push(Span::styled("[subtask] ", t.cmd_desc));
        }
        tool_spans.push(Span::styled(tool.clone(), value_style));

        let scopes: Vec<&str> = scope.split(" && ").collect();
        let mut lines = vec![Line::raw(""), Line::from(tool_spans)];
        for (i, s) in scopes.iter().enumerate() {
            let label = if i == 0 { "scope " } else { "    + " };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(label, label_style),
                Span::styled(s.to_string(), value_style),
            ]));
        }

        let show_allow = scopes
            .iter()
            .zip(generalized_scopes.iter())
            .any(|(s, g)| *s != g.as_str());
        if show_allow {
            for (i, g) in generalized_scopes.iter().enumerate() {
                let label = if i == 0 { "allow " } else { "    + " };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(label, label_style),
                    Span::styled(g.clone(), value_style),
                ]));
            }
        }

        lines.push(Line::raw(""));
        match *state {
            PromptState::ConfirmAllowAlwaysLocal => {
                lines.push(hint_line(CONFIRM_ALLOW_PROJECT_HINTS));
            }
            PromptState::ConfirmAllowAlwaysGlobal => {
                lines.push(hint_line(CONFIRM_ALLOW_ALL_HINTS));
            }
            PromptState::ConfirmAllowSession => {
                lines.push(hint_line(CONFIRM_SESSION_HINTS));
            }
            PromptState::ConfirmDenyAlwaysLocal => {
                lines.push(hint_line(CONFIRM_DENY_PROJECT_HINTS));
            }
            PromptState::ConfirmDenyAlwaysGlobal => {
                lines.push(hint_line(CONFIRM_DENY_ALL_HINTS));
            }
            PromptState::Normal => {
                lines.extend(aligned_hint_rows(&[HINT_ALLOW_ROW, HINT_DENY_ROW]));
            }
        }
        lines.push(Line::raw(""));
        lines
    }

    pub fn view(&self, frame: &mut Frame, area: Rect) {
        if !self.is_open() {
            return;
        }
        let lines = self.build_lines();
        let t = theme::current();
        render_form(&t, " Permission Required ", frame, area, lines, (0, 0));
    }

    pub fn height(&self, width: u16) -> u16 {
        let inner_width = width.saturating_sub(2);
        let lines = self.build_lines();
        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        para.line_count(inner_width) as u16 + 2
    }
}
