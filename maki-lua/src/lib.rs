pub mod agent_autocmd;
mod api;
pub mod docs;
pub mod docs_render;
mod error;
mod hook;
pub mod language;
mod loader;
mod pack;
pub(crate) mod plugin_permissions;
mod runtime;
pub mod session_snapshot;

pub use api::keymap::{KeymapEntry, KeymapReader, KeymapSnapshot};
pub use api::net::set_allowed_private_hosts;
pub use api::options::{OptionSpec, OptionType, PluginOptionSpecs};
pub use api::pack::{Declared, PackOp};
pub use api::plan::{PlanActionInfo, PlanActionReader, PlanActionSnapshot};
pub use api::session::SessionSnapshotFn;
pub use api::util::command::{
    Anchor, Axis, Border, BuiltinAction, Dimension, Edge, FloatConfig, FloatConfigPatch,
    HintReader, HintSnapshot, LuaCommandInfo, LuaCommandReader, ModelRequest, PlanRequest,
    SessionRequest, Split, TaskRequest, TitlePos, UiAction, UiAttachment, UiReply, WinCommand,
    WinEvent, WinView,
};
pub use docs::{DocKind, FnDoc, ModuleDoc, ParamDoc, api_docs};
pub use error::PluginError;
pub use loader::{EventHandle, PERMISSION_NAME_WARNING, PluginHost, SKIPPED_PLUGIN_WARNING};
pub use maki_agent::SessionEndReason;
pub use pack::{
    DeleteTarget, DiscoveredPackage, Discovery, InstallReport, Interaction, MANAGED_GROUP, Origin,
    PackCommand, PackContext, PackPlan, PackPreparation, PackReport, PlannedOp, UpdateOptions,
    UpdateTarget, apply_pack_plan, discover, discover_installed, install_declared, installed_names,
    lockfile_path, prepare_pack_command, sanitize_message, site_dir,
};
pub use plugin_permissions::{Permission, PluginPermissions, Requested};
pub use runtime::{KILL_GRACE, MAX_INFLIGHT_TOOLS, RestoreItem, WARM_TOOL_CAP};
pub use session_snapshot::{SessionQueueSnapshot, SessionSnapshot};

pub mod test_support {
    use crate::KeymapReader;
    use crate::SessionEndReason;
    use crate::api::keymap::{KeymapEntry, KeymapWriter};
    use crate::api::plan::{PlanActionInfo, PlanActionReader, PlanActionWriter};
    use crate::api::util::command::{
        HintEntries, HintReader, HintWriter, LuaCommandInfo, LuaCommandReader, LuaCommandWriter,
    };
    pub use crate::api::util::dispatch::MAX_HOOK_DEPTH;
    use maki_storage::id::MakiId;

    pub struct PlanActionWriterHandle(PlanActionWriter);

    impl PlanActionWriterHandle {
        pub fn publish(&self, actions: Vec<PlanActionInfo>) {
            self.0.publish(actions);
        }
    }

    pub fn plan_action_writer_pair() -> (PlanActionWriterHandle, PlanActionReader) {
        let (writer, reader) = PlanActionWriter::new();
        (PlanActionWriterHandle(writer), reader)
    }

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

    pub fn keymap_reader_with(entries: Vec<KeymapEntry>) -> KeymapReader {
        let (writer, reader) = KeymapWriter::new();
        writer.publish(entries);
        reader
    }
}
