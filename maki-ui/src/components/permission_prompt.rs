use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use maki_agent::permissions::{DEFAULT_DENY_GUIDANCE, PermissionAnswer, generalized_scopes};
use maki_config::ToolKey;

use crate::components::Overlay;
use crate::components::form::render_form;
use crate::components::hint_line;
use crate::components::is_ctrl;
use crate::text_buffer::TextBuffer;
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

// An untrusted folder keeps its project answers in memory, so the rows say
// what the answer really does instead of promising a saved rule.
const HINT_ALLOW_ROW_UNTRUSTED: &[(&str, &str)] = &[
    ("y", "Allow"),
    ("a", "Project (this session)"),
    ("A", "Always (all projects, saved)"),
    ("s", "Session"),
];
const HINT_DENY_ROW_UNTRUSTED: &[(&str, &str)] = &[
    ("n", "Deny"),
    ("d", "Deny project (this session)"),
    ("D", "Deny-always (all, saved)"),
];
const UNTRUSTED_NOTICE: &str = "folder not trusted, project answers last for this session only";
/// Nothing is written into a folder the user declined, so the answer that
/// survives a restart is the global one and the prompt has to name it.
const UNTRUSTED_DURABLE_ANSWER: &str = "use D to save a deny that outlives this session";

const CONFIRM_ALLOW_PROJECT_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm allow-always (project)"),
    ("any", "Cancel"),
];
const CONFIRM_ALLOW_ALL_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm allow-always (all projects)"),
    ("any", "Cancel"),
];
const CONFIRM_SESSION_HINTS: &[(&str, &str)] =
    &[("Enter / y", "Confirm allow (session)"), ("any", "Cancel")];
const CONFIRM_DENY_PROJECT_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm deny-always (project)"),
    ("any", "Cancel"),
];
const CONFIRM_DENY_ALL_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm deny-always (all projects)"),
    ("any", "Cancel"),
];
const CONFIRM_ALLOW_PROJECT_SESSION_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm allow (project, this session)"),
    ("any", "Cancel"),
];
const CONFIRM_DENY_PROJECT_SESSION_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm deny (project, this session)"),
    ("any", "Cancel"),
];

const DENY_GUIDANCE_HINTS: &[(&str, &str)] = &[("Enter", "Deny"), ("Esc", "Cancel")];

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
                    t.tool_dim,
                ));
            }
            Line::from(spans)
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PromptState {
    #[default]
    Normal,
    ConfirmAllowAlwaysProject,
    ConfirmAllowAlwaysGlobal,
    ConfirmAllowSession,
    ConfirmDenyAlwaysProject,
    ConfirmDenyAlwaysGlobal,
    DenyEditing,
}

pub enum PermissionPrompt {
    Closed,
    Open {
        #[allow(dead_code)]
        id: String,
        tool: ToolKey,
        scopes: Vec<String>,
        subagent_id: Option<String>,
        allow_scopes: Vec<String>,
        project_trusted: bool,
        state: PromptState,
        buffer: TextBuffer,
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

    pub fn open(
        &mut self,
        id: String,
        tool: ToolKey,
        scopes: Vec<String>,
        subagent_id: Option<String>,
        project_trusted: bool,
    ) {
        let allow_scopes = generalized_scopes(&tool, &scopes);
        let allow_scopes = if allow_scopes == scopes {
            vec![]
        } else {
            allow_scopes
        };
        *self = Self::Open {
            id,
            tool,
            scopes,
            subagent_id,
            allow_scopes,
            project_trusted,
            state: PromptState::Normal,
            buffer: TextBuffer::new(String::new()),
        };
    }

    pub(crate) fn tool(&self) -> Option<&ToolKey> {
        match self {
            Self::Open { tool, .. } => Some(tool),
            Self::Closed => None,
        }
    }

    pub fn subagent_id(&self) -> Option<&str> {
        match self {
            Self::Open { subagent_id, .. } => subagent_id.as_deref(),
            Self::Closed => None,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<PermissionAnswer> {
        let Self::Open { state, buffer, .. } = self else {
            return None;
        };
        if is_ctrl(&key) && key.code == KeyCode::Char('c') {
            return Some(PermissionAnswer::Deny);
        }
        if *state == PromptState::DenyEditing {
            return match key.code {
                KeyCode::Enter => {
                    let text = buffer.value().trim().to_string();
                    if text.is_empty() {
                        Some(PermissionAnswer::Deny)
                    } else {
                        Some(PermissionAnswer::DenyWithGuidance(text))
                    }
                }
                KeyCode::Esc => {
                    *buffer = TextBuffer::new(String::new());
                    *state = PromptState::Normal;
                    None
                }
                _ => {
                    buffer.handle_key(key);
                    None
                }
            };
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        let confirm_answer = match *state {
            PromptState::ConfirmAllowAlwaysProject => Some(PermissionAnswer::AllowAlwaysProject),
            PromptState::ConfirmAllowAlwaysGlobal => Some(PermissionAnswer::AllowAlwaysGlobal),
            PromptState::ConfirmAllowSession => Some(PermissionAnswer::AllowSession),
            PromptState::ConfirmDenyAlwaysProject => Some(PermissionAnswer::DenyAlwaysProject),
            PromptState::ConfirmDenyAlwaysGlobal => Some(PermissionAnswer::DenyAlwaysGlobal),
            _ => None,
        };
        if let Some(answer) = confirm_answer {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Enter => Some(answer),
                _ => {
                    *state = PromptState::Normal;
                    None
                }
            };
        }
        match key.code {
            KeyCode::Char('y') => Some(PermissionAnswer::AllowOnce),
            KeyCode::Char('n') => {
                *state = PromptState::DenyEditing;
                None
            }
            KeyCode::Char('a') => {
                *state = PromptState::ConfirmAllowAlwaysProject;
                None
            }
            KeyCode::Char('A') => {
                *state = PromptState::ConfirmAllowAlwaysGlobal;
                None
            }
            KeyCode::Char('d') => {
                *state = PromptState::ConfirmDenyAlwaysProject;
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
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        let Self::Open { state, buffer, .. } = self else {
            return false;
        };
        if *state == PromptState::DenyEditing {
            buffer.insert_text(text);
            return true;
        }
        false
    }

    fn build_lines(&self) -> Vec<Line<'static>> {
        let Self::Open {
            tool,
            scopes,
            subagent_id,
            allow_scopes,
            project_trusted,
            state,
            buffer,
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
            tool_spans.push(Span::styled("[subtask] ", t.item_desc));
        }
        tool_spans.push(Span::styled(tool.to_string(), value_style));

        let mut lines = vec![Line::raw(""), Line::from(tool_spans)];
        for (i, s) in scopes.iter().enumerate() {
            let label = if i == 0 { "scope " } else { "    + " };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(label, label_style),
                Span::styled(s.clone(), value_style),
            ]));
        }

        if !allow_scopes.is_empty() {
            for (i, g) in allow_scopes.iter().enumerate() {
                let label = if i == 0 { "allow " } else { "    + " };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(label, label_style),
                    Span::styled(g.clone(), value_style),
                ]));
            }
        }

        if *state == PromptState::DenyEditing {
            let text = buffer.value();
            let (display_text, cursor_pos) = if text.is_empty() {
                (DEFAULT_DENY_GUIDANCE, 0)
            } else {
                (text.as_str(), TextBuffer::char_to_byte(&text, buffer.x()))
            };
            let (before, after) = display_text.split_at(cursor_pos);
            let mut chars = after.chars();
            let cursor_ch = chars.next().unwrap_or(' ');
            let rest: String = chars.collect();

            let mut spans = vec![Span::raw("  "), Span::styled("guide ", label_style)];
            if text.is_empty() {
                spans.push(Span::styled(cursor_ch.to_string(), Style::new().reversed()));
                spans.push(Span::styled(rest, t.tool_dim));
            } else {
                spans.push(Span::raw(before.to_string()));
                spans.push(Span::styled(cursor_ch.to_string(), Style::new().reversed()));
                if !rest.is_empty() {
                    spans.push(Span::raw(rest));
                }
            }
            lines.push(Line::from(spans));
        }

        if !*project_trusted {
            for notice in [UNTRUSTED_NOTICE, UNTRUSTED_DURABLE_ANSWER] {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(notice, t.tool_dim),
                ]));
            }
        }

        lines.push(Line::raw(""));
        match *state {
            PromptState::ConfirmAllowAlwaysProject => {
                lines.push(hint_line(if *project_trusted {
                    CONFIRM_ALLOW_PROJECT_HINTS
                } else {
                    CONFIRM_ALLOW_PROJECT_SESSION_HINTS
                }));
            }
            PromptState::ConfirmAllowAlwaysGlobal => {
                lines.push(hint_line(CONFIRM_ALLOW_ALL_HINTS));
            }
            PromptState::ConfirmAllowSession => {
                lines.push(hint_line(CONFIRM_SESSION_HINTS));
            }
            PromptState::ConfirmDenyAlwaysProject => {
                lines.push(hint_line(if *project_trusted {
                    CONFIRM_DENY_PROJECT_HINTS
                } else {
                    CONFIRM_DENY_PROJECT_SESSION_HINTS
                }));
            }
            PromptState::ConfirmDenyAlwaysGlobal => {
                lines.push(hint_line(CONFIRM_DENY_ALL_HINTS));
            }
            PromptState::DenyEditing => {
                lines.push(hint_line(DENY_GUIDANCE_HINTS));
            }
            PromptState::Normal => {
                let rows: &[&[(&str, &str)]] = if *project_trusted {
                    &[HINT_ALLOW_ROW, HINT_DENY_ROW]
                } else {
                    &[HINT_ALLOW_ROW_UNTRUSTED, HINT_DENY_ROW_UNTRUSTED]
                };
                lines.extend(aligned_hint_rows(rows));
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

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use maki_agent::permissions::PermissionAnswer;
    use maki_config::ToolKey;
    use test_case::test_case;

    use super::{
        CONFIRM_ALLOW_PROJECT_HINTS, CONFIRM_ALLOW_PROJECT_SESSION_HINTS,
        CONFIRM_DENY_PROJECT_HINTS, CONFIRM_DENY_PROJECT_SESSION_HINTS, PermissionPrompt,
        PromptState, UNTRUSTED_DURABLE_ANSWER, UNTRUSTED_NOTICE,
    };

    fn open_trusted(prompt: &mut PermissionPrompt, project_trusted: bool) {
        prompt.open(
            "id".into(),
            ToolKey::native("bash"),
            vec!["execute".into()],
            None,
            project_trusted,
        );
    }

    fn open_prompt() -> PermissionPrompt {
        let mut prompt = PermissionPrompt::new();
        open_trusted(&mut prompt, true);
        prompt
    }

    fn rendered(prompt: &PermissionPrompt) -> String {
        prompt
            .build_lines()
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn ctrl_c_denies() {
        let mut prompt = open_prompt();
        assert_eq!(prompt.handle_key(ctrl_c()), Some(PermissionAnswer::Deny));
        // Also test from editing state
        let mut prompt2 = open_prompt();
        prompt2.handle_key(key(KeyCode::Char('n')));
        prompt2.handle_key(key(KeyCode::Char('t')));
        assert_eq!(prompt2.handle_key(ctrl_c()), Some(PermissionAnswer::Deny));
    }

    #[test]
    fn n_goes_to_deny_editing() {
        let mut prompt = open_prompt();
        assert_eq!(prompt.handle_key(key(KeyCode::Char('n'))), None);
        if let PermissionPrompt::Open { state, .. } = &prompt {
            assert_eq!(*state, PromptState::DenyEditing);
        } else {
            panic!("expected Open");
        }
    }

    #[test]
    fn deny_editing_esc_returns_to_normal() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        prompt.handle_key(key(KeyCode::Char('t')));
        assert_eq!(prompt.handle_key(key(KeyCode::Esc)), None);
        if let PermissionPrompt::Open { state, buffer, .. } = &prompt {
            assert_eq!(*state, PromptState::Normal);
            assert!(buffer.value().is_empty());
        } else {
            panic!("expected Open");
        }
    }

    #[test]
    fn deny_editing_enter_empty_sends_deny() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)),
            Some(PermissionAnswer::Deny)
        );
    }

    #[test]
    fn deny_editing_with_text_sends_guidance() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        prompt.handle_paste("Use cat");
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)),
            Some(PermissionAnswer::DenyWithGuidance("Use cat".into()))
        );
    }

    #[test]
    fn handle_paste_requires_editing_mode() {
        let mut prompt = open_prompt();
        assert!(!prompt.handle_paste("ignored"));
        prompt.handle_key(key(KeyCode::Char('n')));
        assert!(prompt.handle_paste("accepted"));
        if let PermissionPrompt::Open { buffer, .. } = &prompt {
            assert_eq!(buffer.value(), "accepted");
        } else {
            panic!("expected Open");
        }
    }

    #[test]
    fn wildcard_tool_key_opens() {
        let mut prompt = PermissionPrompt::new();
        prompt.open("id".into(), ToolKey::Wildcard, vec![], None, true);
        assert!(matches!(prompt, PermissionPrompt::Open { .. }));
    }

    /// The always-answers are scoped to the project, and the hint has to say so
    /// before the user commits to one. In an untrusted folder the answer only
    /// holds for the session, and the hint says that instead.
    #[test_case(KeyCode::Char('a'), true, PromptState::ConfirmAllowAlwaysProject, CONFIRM_ALLOW_PROJECT_HINTS ; "allow_always")]
    #[test_case(KeyCode::Char('d'), true, PromptState::ConfirmDenyAlwaysProject, CONFIRM_DENY_PROJECT_HINTS ; "deny_always")]
    #[test_case(KeyCode::Char('a'), false, PromptState::ConfirmAllowAlwaysProject, CONFIRM_ALLOW_PROJECT_SESSION_HINTS ; "allow_always_untrusted")]
    #[test_case(KeyCode::Char('d'), false, PromptState::ConfirmDenyAlwaysProject, CONFIRM_DENY_PROJECT_SESSION_HINTS ; "deny_always_untrusted")]
    fn project_answers_confirm_before_they_apply(
        code: KeyCode,
        project_trusted: bool,
        expected: PromptState,
        hints: &[(&str, &str)],
    ) {
        let mut prompt = PermissionPrompt::new();
        open_trusted(&mut prompt, project_trusted);

        assert_eq!(prompt.handle_key(key(code)), None);

        let PermissionPrompt::Open { state, .. } = &prompt else {
            panic!("expected Open");
        };
        assert_eq!(*state, expected);
        let text = rendered(&prompt);
        assert!(text.contains(hints[0].1), "hint missing from: {text}");
    }

    /// The project answers stay on the screen in an untrusted folder, so the
    /// prompt has to explain how long they last and where a lasting deny goes
    /// instead.
    #[test_case(true, false ; "trusted_folder_says_nothing")]
    #[test_case(false, true ; "untrusted_folder_warns")]
    fn untrusted_folder_notice_follows_trust(project_trusted: bool, warned: bool) {
        let mut prompt = PermissionPrompt::new();
        open_trusted(&mut prompt, project_trusted);

        let text = rendered(&prompt);
        assert_eq!(text.contains(UNTRUSTED_NOTICE), warned, "rendered: {text}");
        assert_eq!(
            text.contains(UNTRUSTED_DURABLE_ANSWER),
            warned,
            "rendered: {text}"
        );
        assert_eq!(
            prompt.handle_key(key(KeyCode::Char('a'))),
            None,
            "the project answer stays available either way"
        );
    }
}
