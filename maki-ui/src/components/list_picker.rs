//! Modal list picker with search. Supports immediate (`open`) or lazy loading
//! (`open_loading` → `resolve`) where a spinner is shown until items arrive.

use std::sync::OnceLock;
use std::time::Instant;

use crate::animation::{spinner_frame, spinner_str};
use crate::components::Overlay;
use crate::components::hint_line;
use crate::components::is_ctrl;
use crate::components::keybindings::key;
use crate::components::modal::Modal;
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::text_buffer::TextBuffer;
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const NO_MATCHES: &str = "No matches";
const LOADING_LABEL: &str = "Loading...";
const SEARCH_PREFIX: &str = super::CHEVRON;
const MIN_WIDTH_PERCENT: u16 = 65;
const MAX_HEIGHT_PERCENT: u16 = 80;
const SEARCH_ROW: u16 = 1;
const DETAIL_RIGHT_PAD: u16 = 1;

/// Spinners need a consistent time reference. Using a static epoch avoids
/// passing Instant through every render call.
fn animation_elapsed_ms() -> u128 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis()
}

pub trait PickerItem {
    fn label(&self) -> &str;
    fn detail(&self) -> Option<&str> {
        None
    }
    fn section(&self) -> Option<&str> {
        None
    }
    /// Shows a spinner in the detail slot when true.
    fn is_spinning(&self) -> bool {
        false
    }
}

impl PickerItem for String {
    fn label(&self) -> &str {
        self
    }
}

pub enum PickerAction<T> {
    Consumed,
    Select(usize, T),
    Toggle(usize, bool),
    Close,
}

enum PickerState<T> {
    Loading,
    Ready(State<T>),
}

impl<T> PickerState<T> {
    fn ready(&self) -> Option<&State<T>> {
        match self {
            Self::Ready(s) => Some(s),
            Self::Loading => None,
        }
    }

    fn ready_mut(&mut self) -> Option<&mut State<T>> {
        match self {
            Self::Ready(s) => Some(s),
            Self::Loading => None,
        }
    }
}

pub struct ListPicker<T> {
    state: Option<PickerState<T>>,
    title: &'static str,
    max_visible: Option<u16>,
    generation: u64,
    footer: Option<FooterSpec>,
}

enum FooterSpec {
    Pairs(&'static [(&'static str, &'static str)]),
    Builder(fn() -> Line<'static>),
}

impl FooterSpec {
    fn build(&self) -> Line<'static> {
        match self {
            Self::Pairs(hints) => hint_line(hints),
            Self::Builder(b) => b(),
        }
    }
}

struct State<T> {
    items: Vec<T>,
    filtered: Vec<usize>,
    selected: usize,
    search: TextBuffer,
    scroll_offset: usize,
    viewport_height: usize,
    inner_area: Rect,
    enabled: Option<Vec<bool>>,
}

impl<T: PickerItem> State<T> {
    fn new(items: Vec<T>) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            items,
            filtered,
            selected: 0,
            search: TextBuffer::new(String::new()),
            scroll_offset: 0,
            viewport_height: 20,
            inner_area: Rect::default(),
            enabled: None,
        }
    }

    fn replace_items(&mut self, items: Vec<T>) {
        self.items = items;
        self.rebuild_filter();
        self.clamp_selection();
    }

    fn rebuild_filter(&mut self) {
        let query = self.search.value();
        if query.is_empty() {
            self.filtered = (0..self.items.len()).collect();
        } else {
            let query_lower = query.to_ascii_lowercase();
            self.filtered = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.label().to_ascii_lowercase().contains(&query_lower))
                .map(|(i, _)| i)
                .collect();
        }
    }

    fn clamp_selection(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
            self.scroll_offset = 0;
        } else {
            self.selected = self.selected.min(self.filtered.len() - 1);
            self.scroll_offset = self.scroll_offset.min(self.selected);
        }
    }

    fn update_search_and_clamp(&mut self) {
        self.rebuild_filter();
        self.clamp_selection();
    }

    fn move_up(&mut self) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        self.selected = if self.selected == 0 {
            len - 1
        } else {
            self.selected - 1
        };
        self.ensure_visible();
    }

    fn move_down(&mut self) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        self.selected = if self.selected == len - 1 {
            0
        } else {
            self.selected + 1
        };
        self.ensure_visible();
    }

    fn ensure_visible(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
        let visual = visual_rows_in_range(
            &self.filtered,
            &self.items,
            self.scroll_offset,
            self.selected + 1,
        );
        if visual > self.viewport_height {
            self.scroll_offset = find_scroll_offset_for(
                &self.filtered,
                &self.items,
                self.selected,
                self.viewport_height,
            );
        }
        let max_offset =
            find_scroll_offset_for_bottom(&self.filtered, &self.items, self.viewport_height);
        self.scroll_offset = self.scroll_offset.min(max_offset);
    }

    fn selected_item_index(&self) -> Option<usize> {
        self.filtered.get(self.selected).copied()
    }
}

impl<T: PickerItem> ListPicker<T> {
    pub fn new() -> Self {
        Self {
            state: None,
            title: "",
            max_visible: None,
            generation: 0,
            footer: None,
        }
    }

    pub fn with_max_visible(mut self, max: u16) -> Self {
        self.max_visible = Some(max);
        self
    }

    pub fn with_footer(mut self, hints: &'static [(&'static str, &'static str)]) -> Self {
        self.footer = Some(FooterSpec::Pairs(hints));
        self
    }

    pub fn with_footer_builder(mut self, builder: fn() -> Line<'static>) -> Self {
        self.footer = Some(FooterSpec::Builder(builder));
        self
    }

    pub fn open_toggleable(&mut self, items: Vec<T>, enabled: Vec<bool>, title: &'static str) {
        assert_eq!(
            items.len(),
            enabled.len(),
            "items and enabled must have same length"
        );
        self.generation += 1;
        self.title = title;
        let mut state = State::new(items);
        state.enabled = Some(enabled);
        self.state = Some(PickerState::Ready(state));
    }

    pub fn open(&mut self, items: Vec<T>, title: &'static str) {
        self.generation += 1;
        self.title = title;
        self.state = Some(PickerState::Ready(State::new(items)));
    }

    pub fn open_loading(&mut self, title: &'static str) {
        self.generation += 1;
        self.title = title;
        self.state = Some(PickerState::Loading);
    }

    pub fn resolve(&mut self, items: Vec<T>) {
        if !self.is_loading() {
            return;
        }
        self.generation += 1;
        self.state = Some(PickerState::Ready(State::new(items)));
    }

    pub fn select(&mut self, index: usize) {
        if let Some(s) = self.state.as_mut().and_then(PickerState::ready_mut) {
            self.generation += 1;
            s.selected = index.min(s.filtered.len().saturating_sub(1));
            s.ensure_visible();
        }
    }

    pub fn replace_items(&mut self, items: Vec<T>) {
        if let Some(s) = self.state.as_mut().and_then(PickerState::ready_mut) {
            self.generation += 1;
            s.replace_items(items);
        }
    }

    pub fn replace_toggleable(&mut self, items: Vec<T>, enabled: Vec<bool>) {
        if let Some(s) = self.state.as_mut().and_then(PickerState::ready_mut) {
            self.generation += 1;
            s.enabled = Some(enabled);
            s.replace_items(items);
        }
    }

    pub fn retain(&mut self, f: impl Fn(&T) -> bool) {
        let Some(s) = self.state.as_mut().and_then(PickerState::ready_mut) else {
            return;
        };
        self.generation += 1;
        if let Some(ref mut enabled) = s.enabled {
            let mut new_enabled = Vec::with_capacity(enabled.len());
            let mut i = 0;
            s.items.retain(|item| {
                let keep = f(item);
                if keep {
                    new_enabled.push(enabled[i]);
                }
                i += 1;
                keep
            });
            *enabled = new_enabled;
        } else {
            s.items.retain(|item| f(item));
        }
        if s.items.is_empty() {
            self.state = None;
            return;
        }
        s.rebuild_filter();
        s.clamp_selection();
    }

    pub fn is_open(&self) -> bool {
        self.state.is_some()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn close(&mut self) {
        self.generation += 1;
        self.state = None;
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.state, Some(PickerState::Loading))
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.state
            .as_ref()
            .and_then(PickerState::ready)
            .is_some_and(|s| s.inner_area.contains(pos))
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickerAction<T> {
        match &self.state {
            None => return PickerAction::Close,
            Some(PickerState::Loading) => {
                if key::QUIT.matches(key) || key.code == KeyCode::Esc {
                    self.generation += 1;
                    self.state = None;
                    return PickerAction::Close;
                }
                return PickerAction::Consumed;
            }
            Some(PickerState::Ready(_)) => {}
        }
        self.generation += 1;
        self.handle_ready_key(key)
    }

    fn handle_ready_key(&mut self, key: KeyEvent) -> PickerAction<T> {
        let s = self
            .state
            .as_mut()
            .and_then(PickerState::ready_mut)
            .expect("handle_ready_key called without Ready state");

        if key::QUIT.matches(key) {
            self.state = None;
            return PickerAction::Close;
        }
        if key::DELETE_WORD.matches(key) {
            s.search.remove_word_before_cursor();
            s.update_search_and_clamp();
            return PickerAction::Consumed;
        }
        if is_ctrl(&key) {
            return PickerAction::Consumed;
        }
        match key.code {
            KeyCode::Up => {
                s.move_up();
                PickerAction::Consumed
            }
            KeyCode::Down => {
                s.move_down();
                PickerAction::Consumed
            }
            KeyCode::Enter => {
                let idx = s.selected_item_index();
                if let (Some(enabled), Some(idx)) = (&mut s.enabled, idx) {
                    enabled[idx] = !enabled[idx];
                    return PickerAction::Toggle(idx, enabled[idx]);
                }
                if s.enabled.is_some() {
                    return PickerAction::Consumed;
                }
                match idx {
                    Some(idx) => {
                        let PickerState::Ready(mut state) = self.state.take().unwrap() else {
                            unreachable!("handle_ready_key guarantees Ready state")
                        };
                        PickerAction::Select(idx, state.items.swap_remove(idx))
                    }
                    None => PickerAction::Consumed,
                }
            }
            KeyCode::Esc => {
                self.state = None;
                PickerAction::Close
            }
            KeyCode::Char(c) => {
                s.search.push_char(c);
                s.update_search_and_clamp();
                PickerAction::Consumed
            }
            KeyCode::Backspace => {
                s.search.remove_char();
                s.update_search_and_clamp();
                PickerAction::Consumed
            }
            KeyCode::Left => {
                s.search.move_left();
                PickerAction::Consumed
            }
            KeyCode::Right => {
                s.search.move_right();
                PickerAction::Consumed
            }
            KeyCode::Home => {
                s.search.move_home();
                PickerAction::Consumed
            }
            KeyCode::End => {
                s.search.move_end();
                PickerAction::Consumed
            }
            _ => PickerAction::Consumed,
        }
    }

    pub fn selected_item(&self) -> Option<&T> {
        let s = self.state.as_ref().and_then(PickerState::ready)?;
        s.selected_item_index().map(|i| &s.items[i])
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.state
            .as_ref()
            .and_then(PickerState::ready)
            .and_then(|s| s.selected_item_index())
    }

    pub fn item(&self, idx: usize) -> Option<&T> {
        self.state
            .as_ref()
            .and_then(PickerState::ready)
            .and_then(|s| s.items.get(idx))
    }

    pub fn with_item_mut(&mut self, label: &str, f: impl FnOnce(&mut T)) {
        if let Some(s) = self.state.as_mut().and_then(PickerState::ready_mut)
            && let Some(item) = s.items.iter_mut().find(|i| i.label() == label)
        {
            f(item);
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        let Some(Some(s)) = self.state.as_mut().map(PickerState::ready_mut) else {
            return self.is_open();
        };
        self.generation += 1;
        s.search.insert_text(text);
        s.update_search_and_clamp();
        true
    }

    pub fn scroll(&mut self, delta: i32) {
        let Some(s) = self.state.as_mut().and_then(PickerState::ready_mut) else {
            return;
        };
        self.generation += 1;
        if delta > 0 {
            s.scroll_offset = s.scroll_offset.saturating_sub(delta as usize);
        } else {
            let total_visual = visual_rows_in_range(&s.filtered, &s.items, 0, s.filtered.len());
            let max_offset = if total_visual <= s.viewport_height {
                0
            } else {
                find_scroll_offset_for_bottom(&s.filtered, &s.items, s.viewport_height)
            };
            s.scroll_offset = (s.scroll_offset + delta.unsigned_abs() as usize).min(max_offset);
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let footer = self.footer.as_ref();
        let footer_rows = if footer.is_some() { 1u16 } else { 0 };
        match self.state.as_mut() {
            None => Rect::default(),
            Some(PickerState::Loading) => {
                let modal = Modal {
                    title: self.title,
                    width_percent: MIN_WIDTH_PERCENT,
                    max_height_percent: MAX_HEIGHT_PERCENT,
                };
                let (popup, inner) = modal.render(frame, area, 1 + SEARCH_ROW + footer_rows);
                let constraints: Vec<Constraint> = if footer.is_some() {
                    vec![
                        Constraint::Min(1),
                        Constraint::Length(1),
                        Constraint::Length(1),
                    ]
                } else {
                    vec![Constraint::Min(1), Constraint::Length(1)]
                };
                let areas = Layout::vertical(constraints).split(inner);
                let list_area = areas[0];
                let search_area = areas[1];
                let ch = spinner_frame(animation_elapsed_ms());
                let line = Line::from(Span::styled(
                    format!("  {ch} {LOADING_LABEL}"),
                    theme::current().cmd_desc,
                ));
                frame.render_widget(Paragraph::new(vec![line]), list_area);
                render_search(frame, search_area, &TextBuffer::new(String::new()));
                if let Some(spec) = footer {
                    render_footer(frame, areas[2], spec);
                }
                popup
            }
            Some(PickerState::Ready(s)) => {
                render_ready(frame, area, s, self.title, self.max_visible, footer)
            }
        }
    }
}

impl<T: PickerItem> Overlay for ListPicker<T> {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

fn render_ready<T: PickerItem>(
    frame: &mut Frame,
    area: Rect,
    s: &mut State<T>,
    title: &'static str,
    max_visible: Option<u16>,
    footer: Option<&FooterSpec>,
) -> Rect {
    let footer_rows = if footer.is_some() { 1u16 } else { 0 };
    let content_rows = if s.filtered.is_empty() {
        1
    } else {
        let rows = visual_rows_in_range(&s.filtered, &s.items, 0, s.filtered.len()) as u16;
        match max_visible {
            Some(max) => rows.min(max),
            None => rows,
        }
    };
    let modal = Modal {
        title,
        width_percent: MIN_WIDTH_PERCENT,
        max_height_percent: MAX_HEIGHT_PERCENT,
    };
    let (popup, inner) = modal.render(frame, area, content_rows + SEARCH_ROW + footer_rows);
    let viewport_h = inner.height.saturating_sub(SEARCH_ROW + footer_rows);
    s.viewport_height = viewport_h as usize;
    s.ensure_visible();

    let constraints: Vec<Constraint> = if footer.is_some() {
        vec![
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ]
    } else {
        vec![Constraint::Min(1), Constraint::Length(1)]
    };
    let areas = Layout::vertical(constraints).split(inner);
    let list_area = areas[0];
    let search_area = areas[1];

    render_list(
        frame,
        list_area,
        &s.filtered,
        &s.items,
        s.selected,
        s.scroll_offset,
        s.viewport_height,
        s.enabled.as_deref(),
    );
    render_search(frame, search_area, &s.search);

    if let Some(spec) = footer {
        render_footer(frame, areas[2], spec);
    }

    let total_visual = visual_rows_in_range(&s.filtered, &s.items, 0, s.filtered.len());
    if total_visual as u16 > viewport_h {
        let visual_offset = visual_rows_in_range(&s.filtered, &s.items, 0, s.scroll_offset);
        render_vertical_scrollbar(frame, list_area, total_visual as u16, visual_offset as u16);
    }

    s.inner_area = inner;
    popup
}

fn section_gap<T: PickerItem>(filtered: &[usize], items: &[T], idx: usize) -> usize {
    let item = &items[filtered[idx]];
    let is_break = match item.section() {
        None => false,
        Some(sec) => {
            idx == 0
                || items[filtered[idx - 1]]
                    .section()
                    .is_none_or(|prev| prev != sec)
        }
    };
    if !is_break {
        return 0;
    }
    if idx == 0 { 1 } else { 2 }
}

fn visual_rows_in_range<T: PickerItem>(
    filtered: &[usize],
    items: &[T],
    start: usize,
    end: usize,
) -> usize {
    let item_count = end.saturating_sub(start);
    let section_rows: usize = (start..end).map(|i| section_gap(filtered, items, i)).sum();
    item_count + section_rows
}

fn find_scroll_offset_for<T: PickerItem>(
    filtered: &[usize],
    items: &[T],
    target: usize,
    viewport_height: usize,
) -> usize {
    for start in (0..=target).rev() {
        let rows = visual_rows_in_range(filtered, items, start, target + 1);
        if rows > viewport_height {
            return (start + 1).min(target);
        }
    }
    0
}

fn find_scroll_offset_for_bottom<T: PickerItem>(
    filtered: &[usize],
    items: &[T],
    viewport_height: usize,
) -> usize {
    let len = filtered.len();
    if len == 0 {
        return 0;
    }
    find_scroll_offset_for(filtered, items, len - 1, viewport_height)
}

fn max_label_width(detail: &str, area_width: u16) -> usize {
    area_width.saturating_sub(detail.width() as u16 + 1 + DETAIL_RIGHT_PAD) as usize
}

fn detail_padding(label: &str, detail: &str, area_width: u16) -> usize {
    max_label_width(detail, area_width).saturating_sub(label.width())
}

fn truncate_label(label: &str, max_width: usize) -> String {
    if label.width() <= max_width {
        return label.to_string();
    }
    let target = max_width.saturating_sub(1);
    let mut width = 0;
    let mut result = String::with_capacity(label.len());
    for ch in label.chars() {
        let cw = ch.width().unwrap_or(0);
        if width + cw > target {
            break;
        }
        width += cw;
        result.push(ch);
    }
    result.push('\u{2026}');
    result
}

#[allow(clippy::too_many_arguments)]
fn render_list<T: PickerItem>(
    frame: &mut Frame,
    area: Rect,
    filtered: &[usize],
    items: &[T],
    selected: usize,
    scroll_offset: usize,
    viewport_height: usize,
    enabled: Option<&[bool]>,
) {
    if filtered.is_empty() {
        let line = Line::from(Span::styled(
            format!("  {NO_MATCHES}"),
            theme::current().cmd_desc,
        ));
        frame.render_widget(Paragraph::new(vec![line]), area);
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut i = scroll_offset;
    let mut last_section: Option<&str> = if scroll_offset > 0 && scroll_offset - 1 < filtered.len()
    {
        items[filtered[scroll_offset - 1]].section()
    } else {
        None
    };

    while lines.len() < viewport_height && i < filtered.len() {
        let item_idx = filtered[i];
        let item = &items[item_idx];

        if let Some(sec) = item.section()
            && last_section.is_none_or(|prev| prev != sec)
        {
            if !lines.is_empty() && lines.len() < viewport_height {
                lines.push(Line::raw(""));
            }
            if lines.len() < viewport_height {
                lines.push(Line::from(Span::styled(
                    format!("  {sec}"),
                    theme::current().keybind_section,
                )));
            }
            last_section = Some(sec);
        }

        if lines.len() >= viewport_height {
            break;
        }

        let style = if i == selected {
            theme::current().cmd_selected
        } else {
            theme::current().cmd_name
        };
        let checkbox = enabled.map(|en| {
            let sym = if en[item_idx] { "✓ " } else { "✗ " };
            let sty = if i == selected {
                style
            } else if en[item_idx] {
                theme::current().cmd_name
            } else {
                theme::current().cmd_desc
            };
            Span::styled(sym, sty)
        });
        let label = format!("  {}", item.label());
        let detail: Option<&str> = if item.is_spinning() {
            Some(spinner_str(animation_elapsed_ms()))
        } else {
            item.detail()
        };
        let line = match detail {
            Some(detail) => {
                let label = truncate_label(&label, max_label_width(detail, area.width));
                let pad = detail_padding(&label, detail, area.width);
                let detail_style = if i == selected {
                    style
                } else {
                    theme::current().cmd_desc
                };
                let mut spans = Vec::with_capacity(5);
                if let Some(cb) = checkbox {
                    spans.push(cb);
                }
                spans.push(Span::styled(label, style));
                spans.push(Span::styled(" ".repeat(pad), style));
                spans.push(Span::styled(detail.to_string(), detail_style));
                spans.push(Span::styled(" ".repeat(DETAIL_RIGHT_PAD as usize), style));
                Line::from(spans)
            }
            None => {
                if let Some(cb) = checkbox {
                    Line::from(vec![cb, Span::styled(label, style)])
                } else {
                    Line::from(Span::styled(label, style))
                }
            }
        };
        lines.push(line);
        i += 1;
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_search(frame: &mut Frame, area: Rect, search: &TextBuffer) {
    let query = search.value();
    let cursor_x = search.x();
    let chars: Vec<char> = query.chars().collect();
    let before: String = chars[..cursor_x].iter().collect();
    let cursor_char = chars.get(cursor_x).copied().unwrap_or(' ');
    let after_start = cursor_x.saturating_add(1).min(chars.len());
    let after: String = chars[after_start..].iter().collect();

    let line = Line::from(vec![
        Span::styled(SEARCH_PREFIX, theme::current().picker_search_prefix),
        Span::styled(before, theme::current().picker_search_text),
        Span::styled(cursor_char.to_string(), theme::current().cursor),
        Span::styled(after, theme::current().picker_search_text),
    ]);
    frame.render_widget(Paragraph::new(vec![line]), area);
}

fn render_footer(frame: &mut Frame, area: Rect, spec: &FooterSpec) {
    frame.render_widget(Paragraph::new(vec![spec.build()]), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use crate::components::keybindings::key as kb;
    use crossterm::event::KeyCode;
    use test_case::test_case;

    fn ready_state<T>(p: &ListPicker<T>) -> &State<T> {
        p.state
            .as_ref()
            .and_then(PickerState::ready)
            .expect("expected Ready state")
    }

    fn ready_state_mut<T>(p: &mut ListPicker<T>) -> &mut State<T> {
        p.state
            .as_mut()
            .and_then(PickerState::ready_mut)
            .expect("expected Ready state")
    }

    struct Entry {
        label: String,
        detail: Option<String>,
    }

    impl Entry {
        fn new(label: &str) -> Self {
            Self {
                label: label.into(),
                detail: None,
            }
        }
    }

    impl PickerItem for Entry {
        fn label(&self) -> &str {
            &self.label
        }
        fn detail(&self) -> Option<&str> {
            self.detail.as_deref()
        }
    }

    fn entries(names: &[&str]) -> Vec<Entry> {
        names.iter().map(|n| Entry::new(n)).collect()
    }

    #[test]
    fn navigation_wraps_around() {
        let mut p = ListPicker::new();
        p.open(entries(&["A", "B", "C"]), " Test ");

        p.handle_key(key(KeyCode::Up));
        assert_eq!(ready_state(&p).selected, 2);

        p.handle_key(key(KeyCode::Down));
        assert_eq!(ready_state(&p).selected, 0);
    }

    #[test]
    fn search_filters_progressively() {
        let mut p = ListPicker::new();
        p.open(entries(&["Alpha", "Beta"]), " Test ");
        assert_eq!(ready_state(&p).filtered, vec![0, 1]);

        p.handle_key(key(KeyCode::Char('a')));
        assert_eq!(ready_state(&p).filtered, vec![0, 1]);

        p.handle_key(key(KeyCode::Char('l')));
        assert_eq!(ready_state(&p).filtered, vec![0]);
    }

    #[test]
    fn enter_returns_selected_item() {
        let mut p = ListPicker::new();
        p.open(entries(&["A", "B", "C"]), " Test ");
        p.handle_key(key(KeyCode::Down));

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, PickerAction::Select(1, ref e) if e.label == "B"));
        assert!(!p.is_open());
    }

    #[test_case(key(KeyCode::Esc) ; "esc_returns_close")]
    #[test_case(kb::QUIT.to_key_event() ; "ctrl_c_returns_close")]
    fn cancel_returns_close(cancel_key: KeyEvent) {
        let mut p = ListPicker::new();
        p.open(entries(&["A", "B"]), " Test ");

        let action = p.handle_key(cancel_key);
        assert!(matches!(action, PickerAction::Close));
        assert!(!p.is_open());
    }

    #[test]
    fn enter_on_empty_results_consumed() {
        let mut p = ListPicker::new();
        p.open(entries(&["Alpha"]), " Test ");
        p.handle_key(key(KeyCode::Char('z')));

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, PickerAction::Consumed));
    }

    #[test_case(0, -3, 3  ; "scroll_down")]
    #[test_case(0, 100, 0  ; "clamp_at_top")]
    #[test_case(5, 3, 2    ; "scroll_up")]
    #[test_case(0, -100, 20 ; "clamp_at_bottom")]
    fn scroll_bounds(initial: usize, delta: i32, expected: usize) {
        let items: Vec<Entry> = (0..30).map(|i| Entry::new(&format!("Item {i}"))).collect();
        let mut p = ListPicker::new();
        p.open(items, " Test ");
        let s = ready_state_mut(&mut p);
        s.viewport_height = 10;
        s.scroll_offset = initial;

        p.scroll(delta);
        assert_eq!(ready_state(&p).scroll_offset, expected);
    }

    #[test]
    fn ctrl_w_deletes_word() {
        let mut p = ListPicker::new();
        p.open(entries(&["A", "B"]), " Test ");
        p.handle_key(key(KeyCode::Char('h')));
        p.handle_key(key(KeyCode::Char('i')));
        assert_eq!(ready_state(&p).search.value(), "hi");

        p.handle_key(kb::DELETE_WORD.to_key_event());
        assert_eq!(ready_state(&p).search.value(), "");
    }

    struct SectionEntry {
        label: String,
        section: &'static str,
    }

    impl PickerItem for SectionEntry {
        fn label(&self) -> &str {
            &self.label
        }
        fn section(&self) -> Option<&str> {
            Some(self.section)
        }
    }

    fn section_entries() -> Vec<SectionEntry> {
        vec![
            SectionEntry {
                label: "a1".into(),
                section: "A",
            },
            SectionEntry {
                label: "a2".into(),
                section: "A",
            },
            SectionEntry {
                label: "b1".into(),
                section: "B",
            },
        ]
    }

    #[test]
    fn section_headers_counted_in_visual_rows() {
        let items = section_entries();
        let filtered: Vec<usize> = (0..items.len()).collect();
        let rows = visual_rows_in_range(&filtered, &items, 0, items.len());
        assert_eq!(rows, 6);
    }

    #[test]
    fn section_navigation_accounts_for_headers() {
        let mut p = ListPicker::new();
        p.open(section_entries(), " Test ");
        let s = ready_state_mut(&mut p);
        s.viewport_height = 3;

        s.selected = 2;
        s.ensure_visible();
        assert_eq!(s.scroll_offset, 2);
    }

    #[test]
    fn ensure_visible_clamps_scroll_offset_after_filter() {
        let mut p = ListPicker::new();
        let items: Vec<Entry> = (0..20).map(|i| Entry::new(&format!("Item {i}"))).collect();
        p.open(items, " Test ");
        let s = ready_state_mut(&mut p);
        s.viewport_height = 10;
        s.scroll_offset = 10;
        s.selected = 15;

        s.search.insert_text("0");
        s.update_search_and_clamp();
        s.ensure_visible();
        assert_eq!(s.scroll_offset, 0);
    }

    #[test_case(key(KeyCode::Esc) ; "esc_closes_loading")]
    #[test_case(kb::QUIT.to_key_event() ; "ctrl_c_closes_loading")]
    fn loading_cancel_keys(cancel_key: KeyEvent) {
        let mut p: ListPicker<Entry> = ListPicker::new();
        p.open_loading(" Test ");
        let action = p.handle_key(cancel_key);
        assert!(matches!(action, PickerAction::Close));
        assert!(!p.is_open());
    }

    #[test]
    fn loading_swallows_other_keys() {
        let mut p: ListPicker<Entry> = ListPicker::new();
        p.open_loading(" Test ");
        let action = p.handle_key(key(KeyCode::Char('a')));
        assert!(matches!(action, PickerAction::Consumed));
        assert!(p.is_loading());
    }

    #[test]
    fn resolve_transitions_to_ready() {
        let mut p = ListPicker::new();
        p.open_loading(" Test ");
        p.resolve(entries(&["A", "B"]));
        assert!(p.is_open());
        assert!(!p.is_loading());
        assert_eq!(ready_state(&p).items.len(), 2);
    }

    #[test]
    fn resolve_ignored_when_not_loading() {
        let mut p = ListPicker::new();
        p.open(entries(&["A"]), " Test ");
        let gen_before = p.generation();
        p.resolve(entries(&["B", "C"]));
        assert_eq!(p.generation(), gen_before);
        assert_eq!(ready_state(&p).items.len(), 1);

        let mut p2: ListPicker<Entry> = ListPicker::new();
        p2.resolve(entries(&["A"]));
        assert!(!p2.is_open());
    }

    #[test]
    fn toggle_mode_enter_flips_enabled() {
        let mut p = ListPicker::new();
        p.open_toggleable(entries(&["A", "B"]), vec![true, true], " Test ");
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, PickerAction::Toggle(0, false)));
        assert!(p.is_open());
    }

    #[test]
    fn toggle_mode_search_targets_correct_item() {
        let mut p = ListPicker::new();
        p.open_toggleable(entries(&["Alpha", "Beta"]), vec![true, true], " Test ");
        p.handle_key(key(KeyCode::Char('b')));
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, PickerAction::Toggle(1, false)));
    }

    #[test]
    fn retain_syncs_enabled_vec() {
        let mut p = ListPicker::new();
        p.open_toggleable(entries(&["A", "B", "C"]), vec![true, false, true], " Test ");
        p.retain(|e| e.label() != "B");
        let s = ready_state(&p);
        assert_eq!(s.items.len(), 2);
        assert_eq!(s.enabled.as_ref().unwrap(), &[true, true]);
    }

    #[test]
    fn retain_all_removed_closes_picker() {
        let mut p = ListPicker::new();
        p.open_toggleable(entries(&["A"]), vec![true], " Test ");
        p.retain(|_| false);
        assert!(!p.is_open());
    }

    #[test_case("short", 10 => "short" ; "no_truncation_needed")]
    #[test_case("abcdefghijklmno", 10 => "abcdefghi\u{2026}" ; "long_ascii_truncated")]
    #[test_case("ab\u{4e16}\u{754c}cde", 6 => "ab\u{4e16}\u{2026}" ; "wide_chars_truncated")]
    fn truncate_label_cases(label: &str, max_width: usize) -> String {
        truncate_label(label, max_width)
    }

    #[test]
    fn detail_right_edge_consistent_for_long_and_short_labels() {
        let width: u16 = 40;
        let detail = "2h ago";
        let max_w = max_label_width(detail, width);

        let end_col = |label: &str| -> usize {
            let t = truncate_label(label, max_w);
            t.width()
                + detail_padding(&t, detail, width)
                + detail.width()
                + DETAIL_RIGHT_PAD as usize
        };

        let long = "  ".to_string() + &"x".repeat(60);
        assert_eq!(end_col(&long), end_col("  hi"));
        assert!(end_col(&long) <= width as usize);
    }
}
