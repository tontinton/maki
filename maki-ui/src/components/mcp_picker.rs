use std::sync::Arc;

use arc_swap::ArcSwap;
use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use maki_agent::{McpServerInfo, McpServerStatus};

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};

const TITLE: &str = " MCP Servers ";

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
    infos: Arc<ArcSwap<Vec<McpServerInfo>>>,
}

impl McpPicker {
    pub fn new(infos: Arc<ArcSwap<Vec<McpServerInfo>>>) -> Self {
        Self {
            picker: ListPicker::new(),
            infos,
        }
    }

    pub fn open(&mut self) {
        let guard = self.infos.load();
        let infos: &[McpServerInfo] = &guard;

        let entries: Vec<McpEntry> = infos
            .iter()
            .map(|info| McpEntry {
                name: info.name.clone(),
                detail_text: match &info.status {
                    McpServerStatus::Connecting => {
                        format!("{} \u{00b7} connecting\u{2026}", info.transport_kind)
                    }
                    McpServerStatus::Running => {
                        format!("{} \u{00b7} {} tools", info.transport_kind, info.tool_count)
                    }
                    McpServerStatus::Disabled => {
                        format!("{} \u{00b7} disabled", info.transport_kind)
                    }
                    McpServerStatus::Failed(e) => {
                        format!("{} \u{00b7} error: {}", info.transport_kind, e)
                    }
                },
            })
            .collect();
        let enabled: Vec<bool> = infos.iter().map(|info| info.status.is_active()).collect();
        self.picker.open_toggleable(entries, enabled, TITLE);
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
    use std::path::PathBuf;
    use test_case::test_case;

    fn test_infos() -> Arc<ArcSwap<Vec<McpServerInfo>>> {
        Arc::new(ArcSwap::from_pointee(vec![
            McpServerInfo {
                name: "fs".into(),
                transport_kind: "stdio",
                tool_count: 5,
                status: McpServerStatus::Running,
                config_path: PathBuf::from("/home/.config/maki/config.toml"),
            },
            McpServerInfo {
                name: "github".into(),
                transport_kind: "stdio",
                tool_count: 3,
                status: McpServerStatus::Disabled,
                config_path: PathBuf::from("/project/.maki/config.toml"),
            },
        ]))
    }

    #[test]
    fn toggle_returns_server_name_and_new_state() {
        let mut p = McpPicker::new(test_infos());
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
        let mut p = McpPicker::new(test_infos());
        p.open();
        let action = p.handle_key(cancel_key);
        assert!(matches!(action, McpPickerAction::Close));
        assert!(!p.is_open());
    }

    #[test]
    fn open_with_empty_infos() {
        let infos = Arc::new(ArcSwap::from_pointee(vec![]));
        let mut p = McpPicker::new(infos);
        p.open();
        assert!(p.is_open());
    }
}
