use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use maki_agent::{McpServerInfo, McpServerStatus, McpSnapshotReader};

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};

const TITLE: &str = " MCP Servers ";

fn build_entries(infos: &[McpServerInfo]) -> (Vec<McpEntry>, Vec<bool>) {
    let entries = infos
        .iter()
        .map(|info| McpEntry {
            name: info.name.clone(),
            detail_text: match &info.status {
                McpServerStatus::Connecting => {
                    format!("{} \u{00b7} connecting\u{2026}", info.transport_kind)
                }
                McpServerStatus::Running => {
                    let mut parts = vec![info.transport_kind.to_string()];
                    if info.tool_count > 0 {
                        parts.push(format!("{} tools", info.tool_count));
                    }
                    if info.prompt_count > 0 {
                        parts.push(format!("{} prompts", info.prompt_count));
                    }
                    if info.tool_count == 0 && info.prompt_count == 0 {
                        parts.push("no capabilities".into());
                    }
                    parts.join(" \u{00b7} ")
                }
                McpServerStatus::Disabled => {
                    format!("{} \u{00b7} disabled", info.transport_kind)
                }
                McpServerStatus::Failed(e) => {
                    format!("{} \u{00b7} error: {}", info.transport_kind, e)
                }
                McpServerStatus::NeedsAuth { .. } => {
                    format!("{} \u{00b7} needs auth", info.transport_kind)
                }
            },
        })
        .collect();
    let enabled = infos.iter().map(|info| info.status.is_active()).collect();
    (entries, enabled)
}

pub enum McpPickerAction {
    Consumed,
    Toggle { server_name: String, enabled: bool },
    Close,
}

struct McpEntry {
    name: String,
    detail_text: String,
}

impl PickerItem for McpEntry {
    fn label(&self) -> &str {
        &self.name
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.detail_text)
    }
}

pub struct McpPicker {
    picker: ListPicker<McpEntry>,
    snapshot: McpSnapshotReader,
    last_generation: u64,
}

impl McpPicker {
    pub fn new(snapshot: McpSnapshotReader) -> Self {
        Self {
            picker: ListPicker::new(),
            snapshot,
            last_generation: 0,
        }
    }

    pub fn open(&mut self) {
        let guard = self.snapshot.load();
        self.last_generation = guard.generation;
        let (entries, enabled) = build_entries(&guard.infos);
        self.picker.open_toggleable(entries, enabled, TITLE);
    }

    pub fn refresh(&mut self) {
        if !self.picker.is_open() {
            return;
        }
        let guard = self.snapshot.load();
        if guard.generation == self.last_generation {
            return;
        }
        self.last_generation = guard.generation;
        let (entries, enabled) = build_entries(&guard.infos);
        self.picker.replace_toggleable(entries, enabled);
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> McpPickerAction {
        match self.picker.handle_key(key) {
            PickerAction::Consumed => McpPickerAction::Consumed,
            PickerAction::Toggle(idx, enabled) => {
                let server_name = self
                    .picker
                    .item(idx)
                    .expect("toggle idx valid")
                    .name
                    .clone();
                McpPickerAction::Toggle {
                    server_name,
                    enabled,
                }
            }
            PickerAction::Select(..) | PickerAction::Close => McpPickerAction::Close,
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }
}

impl Overlay for McpPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.picker.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use crate::components::keybindings::key as kb;
    use crossterm::event::{KeyCode, KeyEvent};
    use maki_agent::McpSnapshot;
    use std::path::PathBuf;
    use test_case::test_case;

    fn test_snapshot() -> McpSnapshotReader {
        McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: vec![
                McpServerInfo {
                    name: "fs".into(),
                    transport_kind: "stdio",
                    tool_count: 5,
                    prompt_count: 0,
                    status: McpServerStatus::Running,
                    config_path: PathBuf::from("/home/.config/maki/config.toml"),
                    url: None,
                },
                McpServerInfo {
                    name: "github".into(),
                    transport_kind: "stdio",
                    tool_count: 3,
                    prompt_count: 0,
                    status: McpServerStatus::Disabled,
                    config_path: PathBuf::from("/project/.maki/config.toml"),
                    url: None,
                },
            ],
            prompts: vec![],
            pids: vec![],
            generation: 0,
        })
    }

    #[test]
    fn toggle_returns_server_name_and_new_state() {
        let mut p = McpPicker::new(test_snapshot());
        p.open();
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            action,
            McpPickerAction::Toggle { ref server_name, enabled: false } if server_name == "fs"
        ));
    }

    #[test_case(key(KeyCode::Esc)       ; "esc_closes")]
    #[test_case(kb::QUIT.to_key_event() ; "ctrl_c_closes")]
    fn close_keys(cancel_key: KeyEvent) {
        let mut p = McpPicker::new(test_snapshot());
        p.open();
        let action = p.handle_key(cancel_key);
        assert!(matches!(action, McpPickerAction::Close));
        assert!(!p.is_open());
    }

    #[test]
    fn open_with_empty_infos() {
        let mut p = McpPicker::new(McpSnapshotReader::empty());
        p.open();
        assert!(p.is_open());
    }
}
