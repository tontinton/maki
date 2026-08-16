mod api;
pub mod docs;
pub mod docs_render;
mod error;
pub mod language;
mod loader;
mod pack;
pub(crate) mod plugin_permissions;
mod runtime;

pub use api::keymap::{KeymapEntry, KeymapReader, KeymapSnapshot};
pub use api::net::set_allowed_private_hosts;
pub use api::options::{OptionSpec, OptionType, PluginOptionSpecs};
pub use api::pack::{Declared, PackOp};
pub use api::util::command::{
    Anchor, Axis, Border, BuiltinAction, Dimension, Edge, FloatConfig, FloatConfigPatch,
    HintReader, HintSnapshot, LuaCommandInfo, LuaCommandReader, ModelRequest, SessionRequest,
    Split, TaskRequest, TitlePos, UiAction, UiReply, WinCommand, WinEvent, WinView,
};
pub use docs::{DocKind, FnDoc, ModuleDoc, ParamDoc, api_docs};
pub use error::PluginError;
pub use loader::{EventHandle, PluginHost, SKIPPED_PLUGIN_WARNING};
pub use pack::{
    DiscoveredPackage, Discovery, InstallReport, Interaction, MANAGED_GROUP, Origin, discover,
    discover_installed, install_declared, lockfile_path, sanitize_message, site_dir,
};
pub use plugin_permissions::{Permission, PluginPermissions, Requested};
pub use runtime::{KILL_GRACE, RestoreItem, WARM_TOOL_CAP};

pub mod test_support {
    use crate::KeymapReader;
    use crate::api::keymap::{KeymapEntry, KeymapWriter};
    use crate::api::util::command::{
        HintEntries, HintReader, HintWriter, LuaCommandInfo, LuaCommandReader, LuaCommandWriter,
    };

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

        /// Next `SessionEnd` request as the session being left behind.
        pub fn try_recv_end_session(&self) -> Option<maki_storage::id::MakiId> {
            use crate::runtime::Request;
            while let Ok(req) = self.0.try_recv() {
                if let Request::EndSession { session } = req {
                    return Some(session);
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
