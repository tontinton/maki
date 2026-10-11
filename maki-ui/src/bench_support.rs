//! A bench links this crate from outside, so the few private items it needs
//! to build an `App` and drive its input get a public door here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use maki_agent::permissions::PermissionManager;
use maki_agent::{McpConfigErrors, McpSnapshotReader};
use maki_config::{ModelPolicy, PermissionsConfig, ProjectConfig, UiConfig};
use maki_lua::{EventHandle, HintReader, InputEdit, KeymapReader, LuaCommandReader, TextEdit};
use maki_providers::{Model, ModelTier};
use maki_storage::StateDir;
use ratatui::layout::Rect;

use crate::OpenSession;
use crate::app::App;
use crate::storage_writer::StorageWriter;

const CWD: &str = "/tmp";
const INPUT_HISTORY_SIZE: usize = 100;
const PLUGIN: &str = "bench";
const PROVIDER: &str = "anthropic";

/// No Lua, no MCP and no agent, so the numbers are the UI's own work. The
/// model is a tier default rather than a named one, so the bench keeps
/// building after the catalogue drops a model. {storage} should be a scratch
/// dir, because the fresh session takes a claim in it.
pub fn app(storage: StateDir) -> App {
    let model = Model::from_tier(PROVIDER, ModelTier::Medium).expect("provider has a default");
    let tab = OpenSession::fresh(&model.spec(), CWD, &storage);
    let writer = Arc::new(StorageWriter::new(storage.clone(), flume::unbounded().0));
    let permissions = Arc::new(PermissionManager::new(
        PermissionsConfig::default(),
        PathBuf::from(CWD),
        ProjectConfig::discover(Path::new(CWD)),
        Arc::default(),
    ));
    App::new(
        &model,
        tab,
        storage,
        Arc::default(),
        McpSnapshotReader::empty(),
        McpConfigErrors::new(PathBuf::new()),
        LuaCommandReader::empty(),
        KeymapReader::empty(),
        HintReader::empty(),
        writer,
        UiConfig::default(),
        INPUT_HISTORY_SIZE,
        permissions,
        Arc::from([]),
        EventHandle::disconnected_for_test(),
        Arc::new(ModelPolicy::default()),
    )
}

pub fn input_value(app: &App) -> &str {
    app.input_box.buffer.value()
}

/// Lands the text the way history recall does, with the cursor parked at
/// the start.
pub fn set_input(app: &mut App, text: &str) {
    app.input_box.set_input(text);
}

/// Lands the edit the way `maki.ui.input_edit` does. {area} matters because
/// a terminal that leaves the box no text row refuses plugin writes.
pub fn plugin_edit(
    app: &mut App,
    area: Rect,
    start: usize,
    stop: usize,
    text: &str,
) -> Result<(), String> {
    let edit = InputEdit {
        edits: vec![TextEdit {
            start,
            stop,
            text: text.to_owned(),
        }],
        cursor: None,
        version: app.input_box.buffer.version(),
        strict: false,
        session_id: app.state.session.id.to_string(),
        plugin: Arc::from(PLUGIN),
    };
    app.apply_input_edit(edit, area).map(drop)
}

/// Parks the cursor on {line} at column {x}.
pub fn set_cursor(app: &mut App, line: usize, x: usize) {
    app.input_box.buffer.set_cursor(line, x);
}
