//! Drives a Lua picker plugin against a fake `maki`, pressing scripted keys,
//! so its key handling is tested without a terminal.

use mlua::{Function, Lua};

pub(crate) const DOWN: &str = "<Down>";
pub(crate) const UP: &str = "<Up>";
pub(crate) const PAGE_DOWN: &str = "<PageDown>";
pub(crate) const ESC: &str = "<Esc>";

const PICKER_LIBS: [(&str, &str); 4] = [
    (
        "maki.list_nav",
        include_str!("../../../../plugins/lib/maki/list_nav.lua"),
    ),
    (
        "maki.text_input",
        include_str!("../../../../plugins/lib/maki/text_input.lua"),
    ),
    (
        "maki.color",
        include_str!("../../../../plugins/lib/maki/color.lua"),
    ),
    (
        "maki.list_picker",
        include_str!("../../../../plugins/lib/maki/list_picker.lua"),
    ),
];
const PICKER_PRELUDE: &str = "
    local modules = {}
    function require(name) return assert(modules[name], name) end
    function install_module(name, source) modules[name] = assert(loadstring(source))() end
    function fake_win(keys)
        local win = {width = 80, height = 20}
        function win:set_config(_) end
        function win:set_cursor(_) end
        function win:close() end
        function win:recv(_)
            return {type = 'key', key = assert(table.remove(keys, 1), 'picker did not close')}
        end
        return win
    end
";

/// Installs the shared picker libraries and {modules} behind a fake
/// `require`, runs {fake_maki} to define the `maki` global, then loads
/// {plugin}. `fake_win(picker_keys)` hands {fake_maki} a window that
/// answers {keys} in order and fails once they run out.
pub(crate) fn load_picker(
    lua: &Lua,
    keys: &[&str],
    fake_maki: &str,
    modules: &[(&str, &str)],
    plugin: &str,
) {
    let globals = lua.globals();
    globals
        .set(
            "picker_keys",
            lua.create_sequence_from(keys.iter().copied()).unwrap(),
        )
        .unwrap();
    lua.load(PICKER_PRELUDE).exec().unwrap();
    lua.load(fake_maki).exec().unwrap();
    let install: Function = globals.get("install_module").unwrap();
    for (name, source) in PICKER_LIBS.iter().chain(modules) {
        install.call::<()>((*name, *source)).unwrap();
    }
    lua.load(plugin).exec().unwrap();
}
