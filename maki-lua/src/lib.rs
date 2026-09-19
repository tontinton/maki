pub mod agent_autocmd;
mod api;
pub mod docs;
pub mod docs_render;
mod error;
mod hook;
pub mod key;
mod key_lint;
pub mod language;
mod loader;
mod pack;
pub(crate) mod plugin_permissions;
mod runtime;
pub mod session_snapshot;

pub use api::keymap::{KeybindTicket, KeymapEntry, KeymapReader, KeymapSnapshot};
pub use api::net::set_allowed_private_hosts;
pub use api::options::{OptionSpec, OptionType, PluginOptionSpecs};
pub use api::pack::{Declared, PackOp};
pub use api::session::SessionSnapshotFn;
pub use api::util::command::{
    Anchor, Axis, Border, BuiltinAction, Dimension, Edge, FloatConfig, FloatConfigPatch,
    HintReader, HintSnapshot, InputEdit, InputRequest, LuaCommandInfo, LuaCommandReader,
    ModelRequest, PlanActionOutcome, PlanFormRow, PlanMenu, PlanRequest, PlanRowAction,
    SessionRequest, Split, TaskRequest, TitlePos, UiAction, UiAttachment, UiReply, WinCommand,
    WinEvent, WinView,
};
pub use docs::{DocKind, FnDoc, ModuleDoc, ParamDoc, api_docs};
pub use error::PluginError;
pub use key::{Key, RESERVED_KEYS, is_reserved};
pub use key_lint::KEY_WARNING;
pub use loader::{
    EventHandle, InitFiles, PERMISSION_NAME_WARNING, PluginHost, SKIPPED_PLUGIN_WARNING,
};
pub use maki_agent::SessionEndReason;
pub use pack::{
    DeleteTarget, DiscoveredPackage, Discovery, InstallReport, Interaction, MANAGED_GROUP, Origin,
    PackCommand, PackContext, PackPlan, PackPreparation, PackReport, PlannedOp, UpdateOptions,
    UpdateTarget, apply_pack_plan, discover, discover_installed, install_declared, installed_names,
    lockfile_path, prepare_pack_command, sanitize_message, site_dir,
};
pub use plugin_permissions::{Permission, PluginPermissions, Requested};
pub use runtime::{
    KILL_GRACE, MAX_INFLIGHT_TOOLS, PLAN_FORM_SLOT_DEADLINE, PLAN_ROW_HANDLER_DEADLINE,
    RestoreItem, RestoreReason, WARM_TOOL_CAP,
};
pub use session_snapshot::{SessionQueueSnapshot, SessionSnapshot};

pub mod test_support {
    use std::sync::Arc;

    use crate::KeymapReader;
    use crate::SessionEndReason;
    use crate::api::keymap::KeymapWriter;
    use crate::api::util::command::{
        HintEntries, HintReader, HintWriter, LuaCommandInfo, LuaCommandReader, LuaCommandWriter,
    };
    pub use crate::api::util::dispatch::MAX_HOOK_DEPTH;
    use crate::key::Key;
    use maki_storage::id::MakiId;

    const TEST_PLUGIN: &str = "test-plugin";

    pub struct LuaCommandWriterHandle(LuaCommandWriter);

    impl LuaCommandWriterHandle {
        pub fn publish(&self, commands: Vec<LuaCommandInfo>) {
            self.0.publish(commands);
        }
    }

    pub fn lua_command_writer_pair() -> (LuaCommandWriterHandle, LuaCommandReader) {
        let (writer, reader) = LuaCommandWriter::new();
        (LuaCommandWriterHandle(writer), reader)
    }

    /// Stands in for the Lua thread publishing a plugin's status hints.
    pub struct HintWriterHandle(HintWriter);

    impl HintWriterHandle {
        pub fn publish(&self, entries: HintEntries) {
            self.0.publish(entries);
        }
    }

    pub fn hint_writer_pair() -> (HintWriterHandle, HintReader) {
        let (writer, reader) = HintWriter::new();
        (HintWriterHandle(writer), reader)
    }

    /// Observes which requests an [`crate::EventHandle`] sends, without a
    /// running plugin host.
    pub struct RequestProbe(flume::Receiver<crate::runtime::Request>);

    impl RequestProbe {
        /// Next request as `(kind, clicks)`: `"click"` carries no clicks,
        /// `"click_fallback"` and `"restore"` carry their restore item's.
        pub fn try_recv(&self) -> Option<(&'static str, Vec<usize>)> {
            use crate::runtime::Request;
            Some(match self.0.try_recv().ok()? {
                Request::ClickTool { fallback: None, .. } => ("click", Vec::new()),
                Request::ClickTool {
                    fallback: Some(fb), ..
                } => ("click_fallback", fb.item.clicks),
                Request::RestoreToolAsync { item, .. } => ("restore", item.clicks),
                _ => ("other", Vec::new()),
            })
        }

        /// Next dispatched slash command as `(command, args, depth)`, skipping
        /// other requests.
        pub fn try_recv_command(&self) -> Option<(String, String, u8)> {
            use crate::runtime::Request;
            while let Ok(req) = self.0.try_recv() {
                if let Request::RunCommand {
                    command,
                    args,
                    depth,
                    ..
                } = req
                {
                    return Some((command.to_string(), args, depth));
                }
            }
            None
        }

        /// Next key handed to a plugin binding, skipping other requests. The
        /// chat input fires an autocmd of its own on every keystroke, so a
        /// test that types cannot tell a dispatched binding from an announced
        /// edit without this.
        pub fn try_recv_keybind(&self) -> Option<Key> {
            use crate::runtime::Request;
            while let Ok(req) = self.0.try_recv() {
                if let Request::RunKeybindCallback { ticket } = req {
                    return Some(ticket.key());
                }
            }
            None
        }

        /// Next queued restore item, skipping other requests.
        pub fn try_recv_restore_item(&self) -> Option<crate::RestoreItem> {
            use crate::runtime::Request;
            while let Ok(req) = self.0.try_recv() {
                if let Request::RestoreToolAsync { item, .. } = req {
                    return Some(item);
                }
            }
            None
        }

        /// Next fired autocmd as `(event, data)`, skipping other requests.
        pub fn try_recv_autocmd(&self) -> Option<(String, serde_json::Value)> {
            use crate::runtime::Request;
            while let Ok(req) = self.0.try_recv() {
                if let Request::FireAutocmd { event, data } = req {
                    return Some((event, data));
                }
            }
            None
        }

        /// Next `SessionEnd` request as the session being left behind and why.
        pub fn try_recv_end_session(&self) -> Option<(MakiId, SessionEndReason)> {
            use crate::runtime::Request;
            while let Ok(req) = self.0.try_recv() {
                if let Request::EndSession(end) = req {
                    return Some((end.session, end.reason));
                }
            }
            None
        }
    }

    pub fn probed_event_handle() -> (crate::EventHandle, RequestProbe) {
        let (tx, rx) = flume::unbounded();
        (crate::EventHandle::probed_for_test(tx), RequestProbe(rx))
    }

    /// [`probed_event_handle`] for a host with a plugin layering the plan
    /// form, the one case the UI has to ask a chain before it draws.
    pub fn probed_event_handle_layering_plan_form() -> (crate::EventHandle, RequestProbe) {
        let (handle, probe) = probed_event_handle();
        (handle.layering(&[crate::api::slot::PLAN_FORM_SLOT]), probe)
    }

    /// Publishes {binds} as one plugin's global keymap. The Lua state the
    /// callbacks come from is dropped here: a host test hands the binding back
    /// to a probe instead of calling it.
    pub fn keymap_reader_with(binds: Vec<Key>) -> KeymapReader {
        let lua = mlua::Lua::new();
        let mut store = crate::api::keymap::KeymapStore::new();
        let plugin: Arc<str> = Arc::from(TEST_PLUGIN);
        for key in binds {
            let callback = lua
                .create_registry_value(lua.create_function(|_, ()| Ok(())).unwrap())
                .unwrap();
            store.set(key, callback, Arc::clone(&plugin), String::new(), false);
        }
        let (writer, reader) = KeymapWriter::new();
        writer.publish(store.snapshot_entries());
        reader
    }
}
