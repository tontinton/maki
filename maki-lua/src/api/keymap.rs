use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use arc_swap::ArcSwap;
use maki_agent::{SharedBuf, UiWaker};
use maki_config::Permission;
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{AppDataRefMut, Lua, RegistryKey, Result as LuaResult, Table, Value};
use serde::Serialize;

use crate::api::action::ActionInfo;
use crate::api::ui::buf::BufHandle;
use crate::api::util::convert::TableExt;
use crate::api::util::pair::{Pair, pair};
use crate::key::Key;
use crate::plugin_permissions::{PluginPermissions, require};
use crate::runtime::plugin_loaded;

static NEXT_KEYMAP_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_TICKET_ID: AtomicU64 = AtomicU64::new(1);

const NO_STORE_ERR: &str = "keymap store not initialized";
const RESERVED_KEY_ERR: &str = "is reserved by the host and would never reach this binding";
pub(crate) const TAKEN_ERR: &str = "is already mapped by";
const UNKNOWN_MODE_ERR: &str = "unsupported mode";
const MODE_TYPE_ERR: &str = "mode must be a string or a list of strings";
const NO_MODES_ERR: &str = "mode list is empty";
const BUFFER_TYPE_ERR: &str = "buffer must be a buffer from maki.ui.buf()";
const NORMAL: &str = "n";
const INSERT: &str = "i";
/// One mode for the whole app, like in Neovim. `Insert` means typed keys land
/// in the chat input. `Normal` covers the rest: a plugin switched the input
/// to normal mode, or something else has the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Insert,
}

impl InputMode {
    pub fn letter(self) -> &'static str {
        match self {
            Self::Normal => NORMAL,
            Self::Insert => INSERT,
        }
    }

    pub(crate) fn parse(letter: &str) -> LuaResult<Self> {
        match letter {
            NORMAL => Ok(Self::Normal),
            INSERT => Ok(Self::Insert),
            _ => Err(mlua::Error::runtime(format!(
                "{UNKNOWN_MODE_ERR}: {letter}"
            ))),
        }
    }
}

/// The mode lives on two threads. A plugin picks it with `set_mode` on the
/// Lua thread, but only the UI knows if the chat input has the keyboard.
/// Each half is stored the moment it changes, so `maki.fn.mode()` right after
/// `maki.ui.set_mode` already sees the new mode, and so does the next key.
/// Without a UI nobody stores the focus, so the mode stays `Normal`.
#[derive(Default)]
pub(crate) struct ModeState {
    normal: AtomicBool,
    input_focused: AtomicBool,
}

impl ModeState {
    pub(crate) fn set(&self, mode: InputMode) {
        self.normal
            .store(mode == InputMode::Normal, Ordering::Release);
    }

    pub(crate) fn get(&self) -> InputMode {
        if self.input_focused.load(Ordering::Acquire) && !self.normal.load(Ordering::Acquire) {
            InputMode::Insert
        } else {
            InputMode::Normal
        }
    }
}

/// The buffer a binding is scoped to, `None` for a global binding. Weak, so a
/// binding never keeps its buffer alive. A weak pointer also keeps the memory
/// from being reused, so comparing addresses never takes a new buffer for one
/// that is gone.
type Scope = Option<Weak<SharedBuf>>;

fn scope_ptr(scope: &Scope) -> Option<*const SharedBuf> {
    scope.as_ref().map(Weak::as_ptr)
}

/// The chat input, as `maki.ui.input` reports it.
#[derive(Serialize)]
pub struct InputSnapshot {
    pub session_id: String,
    pub text: String,
    pub cursor: usize,
    pub version: u64,
}

impl InputSnapshot {
    pub(crate) fn to_lua(&self, lua: &Lua) -> LuaResult<Table> {
        let table = lua.create_table_with_capacity(0, 4)?;
        table.raw_set("session_id", self.session_id.as_str())?;
        table.raw_set("text", self.text.as_str())?;
        table.raw_set("cursor", self.cursor)?;
        table.raw_set("version", self.version)?;
        Ok(table)
    }
}

/// A keystroke on its way to the Lua thread. The UI holds the keys typed
/// after it until the Lua thread reports this ticket's handler done.
pub struct KeybindTicket {
    id: u64,
    callback: Arc<RegistryKey>,
    live: Arc<AtomicBool>,
    plugin: Arc<str>,
    grants: PluginPermissions,
    key: Key,
    input: Option<InputSnapshot>,
    text: Option<String>,
}

impl KeybindTicket {
    /// Refuses the key when the plugin's load is gone, so the host runs the
    /// built-in binding in the same key turn rather than handing the key to a
    /// callback that cannot answer it.
    fn claim(entry: &KeymapEntry, key: Key) -> Option<Self> {
        entry.live.load(Ordering::Acquire).then(|| Self {
            id: NEXT_TICKET_ID.fetch_add(1, Ordering::Relaxed),
            callback: Arc::clone(&entry.callback),
            live: Arc::clone(&entry.live),
            plugin: Arc::clone(&entry.plugin),
            grants: entry.grants.clone(),
            key,
            input: None,
            text: None,
        })
    }

    /// Names this keystroke in the done report the Lua thread sends back, so
    /// a slow handler finishing late cannot release the keys a later one holds.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The chat input as it stood when the key was pressed, handed to the
    /// handler as `ev.input` if it may read the session.
    pub fn with_input(self, input: InputSnapshot) -> Self {
        Self {
            input: Some(input),
            ..self
        }
    }

    pub fn take_input(&mut self) -> Option<InputSnapshot> {
        self.input.take()
    }

    /// The pasted text of a `<Paste>`, handed to the handler as `ev.text` if
    /// it may read the session. Cleaned by the host the way the chat input
    /// cleans what lands in it.
    pub fn with_text(self, text: String) -> Self {
        Self {
            text: Some(text),
            ..self
        }
    }

    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    pub fn callback(&self) -> &RegistryKey {
        &self.callback
    }

    /// Whether the load that registered this binding is still the one in
    /// place. Read again on the Lua thread: a `/reload` can land between the
    /// claim and the call, and the plugin it tore down must run nothing.
    pub fn plugin_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }

    /// The load that registered the binding. An [`Arc`] rather than a borrow
    /// because the logs that name it run after the ticket has been handed on.
    pub fn plugin(&self) -> &Arc<str> {
        &self.plugin
    }

    pub(crate) fn grants(&self) -> &PluginPermissions {
        &self.grants
    }

    pub fn key(&self) -> Key {
        self.key
    }
}

/// Carries what the key resolves to, handed to the Lua thread whole.
/// Resolving by id over there instead can find nothing, which leaves the UI
/// having consumed a key with nothing to act on it.
#[derive(Clone, Debug)]
pub struct KeymapEntry {
    pub key: Key,
    pub mode: InputMode,
    pub desc: String,
    pub plugin: Arc<str>,
    pub id: u64,
    buffer: Scope,
    grants: PluginPermissions,
    /// Dropping the last reference hands the registry slot back to mlua, which
    /// the next binding reuses, so a callback an in-flight keystroke still
    /// holds frees itself once that keystroke is done.
    callback: Arc<RegistryKey>,
    /// Shared by every binding of one plugin, and cleared when its load is
    /// torn down.
    live: Arc<AtomicBool>,
}

impl KeymapEntry {
    /// Bound for every window, not only those showing one buffer.
    pub fn is_global(&self) -> bool {
        self.buffer.is_none()
    }

    fn is_for(&self, key: Key, mode: InputMode, buffer: Option<*const SharedBuf>) -> bool {
        self.key == key && self.mode == mode && scope_ptr(&self.buffer) == buffer
    }
}

#[derive(Clone, Default)]
pub struct KeymapSnapshot {
    pub entries: Vec<KeymapEntry>,
    pub generation: u64,
}

#[derive(Clone)]
pub struct KeymapReader {
    snapshot: Arc<ArcSwap<KeymapSnapshot>>,
    pub(crate) mode: Arc<ModeState>,
    actions: Arc<ArcSwap<Vec<ActionInfo>>>,
}

impl KeymapReader {
    pub fn empty() -> Self {
        KeymapWriter::new().1
    }

    pub fn load(&self) -> arc_swap::Guard<Arc<KeymapSnapshot>> {
        self.snapshot.load()
    }

    /// The plugin actions, sorted by name.
    pub fn actions(&self) -> arc_swap::Guard<Arc<Vec<ActionInfo>>> {
        self.actions.load()
    }

    /// Stores whether the chat input has the keyboard and answers the mode a
    /// key is matched in. The UI calls this right before it matches a key, so
    /// a handler asking `maki.fn.mode()` sees the mode its own key matched.
    pub fn sync_mode(&self, input_focused: bool) -> InputMode {
        self.mode
            .input_focused
            .store(input_focused, Ordering::Release);
        self.mode.get()
    }

    /// Hands {key} to the binding on it in {mode} and reports whether it was
    /// taken. With {buffer}, only the bindings set for that buffer are asked,
    /// which the host does for each window showing one before it asks the
    /// global ones with `None`.
    ///
    /// A key nothing took is reported untaken, and the host goes on to the
    /// next step for it in the same key turn. That is the whole of
    /// fall-through: no key is ever handed back afterwards, because a
    /// keystroke replayed into a UI that has moved on lands somewhere the user
    /// never aimed it. A plugin whose load is gone is a plugin that took
    /// nothing, settled here before any Lua runs.
    pub fn dispatch(
        &self,
        key: Key,
        mode: InputMode,
        buffer: Option<&Arc<SharedBuf>>,
        run: impl FnOnce(KeybindTicket) -> bool,
    ) -> bool {
        let buffer = buffer.map(Arc::as_ptr);
        let snapshot = self.snapshot.load();
        let ticket = snapshot
            .entries
            .iter()
            .find(|e| e.is_for(key, mode, buffer))
            .and_then(|entry| KeybindTicket::claim(entry, key));
        ticket.is_some_and(run)
    }
}

pub(crate) struct KeymapWriter {
    store: Arc<ArcSwap<KeymapSnapshot>>,
    generation: AtomicU64,
    pub(crate) mode: Arc<ModeState>,
    actions: Arc<ArcSwap<Vec<ActionInfo>>>,
}

impl KeymapWriter {
    pub fn new() -> (Self, KeymapReader) {
        let inner = Arc::new(ArcSwap::from_pointee(KeymapSnapshot::default()));
        let mode = Arc::<ModeState>::default();
        let actions = Arc::new(ArcSwap::from_pointee(Vec::new()));
        (
            Self {
                store: Arc::clone(&inner),
                generation: AtomicU64::new(0),
                mode: Arc::clone(&mode),
                actions: Arc::clone(&actions),
            },
            KeymapReader {
                snapshot: inner,
                mode,
                actions,
            },
        )
    }

    pub fn publish_actions(&self, actions: Vec<ActionInfo>) {
        self.actions.store(Arc::new(actions));
    }

    pub fn publish(&self, entries: Vec<KeymapEntry>) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.store.store(Arc::new(KeymapSnapshot {
            entries,
            generation,
        }));
    }
}

#[derive(Default)]
pub(crate) struct SetOpts {
    pub desc: String,
    pub unique: bool,
    pub buffer: Scope,
}

/// What a `set` did. The caller does one thing per variant: nothing, warn,
/// fail or log.
pub(crate) enum SetOutcome {
    Free,
    /// The named owner's binding is now underneath and comes back when this
    /// one goes away.
    Shadowed(Arc<str>),
    /// `unique` was asked for and the named owner holds the key. Nothing was
    /// stored.
    Taken(Arc<str>),
    /// The load that called this is gone. Nothing was stored.
    Stale,
}

/// Bindings, global and buffer-local together, newest first, and that is the
/// only ordering in this file: [`Self::set`] inserts at the front,
/// [`Self::snapshot_entries`] copies in order, and
/// [`KeymapReader::dispatch`] takes the first match in its scope.
pub(crate) struct KeymapStore {
    bindings: Vec<KeymapEntry>,
    plugins: HashMap<Arc<str>, Arc<AtomicBool>>,
    /// The plugin that last switched the input to normal mode.
    normal_owner: Option<Arc<str>>,
}

impl KeymapStore {
    pub fn new() -> Self {
        Self {
            bindings: Vec::new(),
            plugins: HashMap::new(),
            normal_owner: None,
        }
    }

    /// The liveness every binding of {plugin} shares. A name that
    /// [`Self::clear_plugin`] tombstoned gets that dead state back rather than
    /// a fresh live one: between a `/reload` and the load that replaces it, a
    /// handler of the load that is gone can still be running and still call
    /// `set`, and the `clear_plugin` that would have taken those bindings away
    /// has already run.
    fn plugin_live(&mut self, plugin: &Arc<str>) -> Arc<AtomicBool> {
        let live = self
            .plugins
            .entry(Arc::clone(plugin))
            .or_insert_with(|| Arc::new(AtomicBool::new(true)));
        Arc::clone(live)
    }

    /// Puts {plugin}'s binding for {key} on top of whatever else holds it in
    /// the same scope, in each of {modes}.
    ///
    /// Every mode gets an entry of its own sharing the one callback, so a
    /// `del` in one mode leaves the others bound, as in Neovim.
    ///
    /// Bindings stack per owner, mode and scope: the caller's own entry for
    /// the key is replaced, everyone else's is shadowed and comes back when
    /// this one goes away. A plugin cannot hold another plugin's
    /// `RegistryKey`, so the save-and-restore vim leaves to `maparg()` has to
    /// be automatic here. A buffer's bindings and the global ones never shadow
    /// each other, because the host asks them at different steps.
    ///
    /// With `unique`, any existing binding for the key in any of {modes} and
    /// the same scope fails the whole call, including the caller's own, which
    /// is what `:map <unique>` means. A reload cannot trip on that because
    /// [`Self::revive`] clears the old load's entries first.
    ///
    /// A `set` from a load that is gone stores nothing. Its binding could never
    /// fire, and on top of a live binding from another plugin it would make
    /// that key fall through to the host until the next reload.
    ///
    /// The bindings of buffers that are gone are dropped here, since none of
    /// them can fire again and their callbacks hold registry slots.
    pub fn set(
        &mut self,
        key: Key,
        modes: &[InputMode],
        callback: RegistryKey,
        plugin: Arc<str>,
        grants: PluginPermissions,
        opts: SetOpts,
    ) -> SetOutcome {
        let live = self.plugin_live(&plugin);
        if !live.load(Ordering::Acquire) {
            return SetOutcome::Stale;
        }
        self.bindings
            .retain(|b| b.buffer.as_ref().is_none_or(|buf| buf.strong_count() > 0));
        let buffer = &opts.buffer;
        if opts.unique
            && let Some(owner) = modes.iter().find_map(|&m| self.owner_of(key, m, buffer))
        {
            return SetOutcome::Taken(owner);
        }
        let callback = Arc::new(callback);
        let mut shadowed = None;
        for &mode in modes {
            self.bindings
                .retain(|b| !(b.is_for(key, mode, scope_ptr(buffer)) && b.plugin == plugin));
            shadowed = shadowed.or_else(|| self.owner_of(key, mode, buffer));
            self.bindings.insert(
                0,
                KeymapEntry {
                    key,
                    mode,
                    desc: opts.desc.clone(),
                    plugin: Arc::clone(&plugin),
                    id: NEXT_KEYMAP_ID.fetch_add(1, Ordering::Relaxed),
                    buffer: buffer.clone(),
                    grants: grants.clone(),
                    callback: Arc::clone(&callback),
                    live: Arc::clone(&live),
                },
            );
        }
        shadowed.map_or(SetOutcome::Free, SetOutcome::Shadowed)
    }

    /// Only the caller's own entry in {buffer}'s scope goes, so whatever it
    /// shadowed comes back and whatever sits on top stays. A plugin never
    /// removes another plugin's binding. If it could, a key would stop working
    /// because some unrelated plugin cleaned up, and nobody could tell why.
    ///
    /// Returns whether the caller had a binding to remove in any of {modes}.
    pub fn del(&mut self, key: Key, modes: &[InputMode], plugin: &str, buffer: &Scope) -> bool {
        let before = self.bindings.len();
        self.bindings.retain(|b| {
            !(b.plugin.as_ref() == plugin
                && modes.iter().any(|&m| b.is_for(key, m, scope_ptr(buffer))))
        });
        self.bindings.len() != before
    }

    fn owner_of(&self, key: Key, mode: InputMode, buffer: &Scope) -> Option<Arc<str>> {
        self.bindings
            .iter()
            .find(|b| b.is_for(key, mode, scope_ptr(buffer)))
            .map(|b| Arc::clone(&b.plugin))
    }

    /// The load is marked dead as well as emptied of keys. The snapshot loses
    /// its bindings either way, but a keystroke already on its way to the Lua
    /// thread carries its callback with it, and that callback belongs to the
    /// chunk this tore down.
    ///
    /// The name is tombstoned rather than forgotten. A keybind handler holds
    /// the drain barrier for a bounded time only, so a `/reload` can land
    /// while one is still running. That handler can call `set`, and a
    /// forgotten name would hand it a fresh live state and publish bindings
    /// for a load that is gone, which the `clear_plugin` that already ran will
    /// never clear. [`Self::revive`] is the one way back.
    pub fn clear_plugin(&mut self, plugin: &str) {
        self.bindings.retain(|b| b.plugin.as_ref() != plugin);
        if let Some(live) = self.plugins.get(plugin) {
            live.store(false, Ordering::Release);
        }
    }

    /// Takes the tombstone off {plugin}, for the load that is about to
    /// register its bindings. Called before that load's chunks run and
    /// nowhere else, which is what tells a new load apart from a handler of
    /// the old one. Anything the old load's stragglers published in between
    /// goes with it: those bindings are dead and belong to nothing.
    pub fn revive(&mut self, plugin: &str) {
        self.bindings.retain(|b| b.plugin.as_ref() != plugin);
        self.plugins.remove(plugin);
    }

    /// Every binding, newest first, which is dispatch order and the order a
    /// keymap listing reads them in: the first entry per key and scope is the
    /// one that fires.
    pub fn snapshot_entries(&self) -> Vec<KeymapEntry> {
        self.bindings.clone()
    }

    /// The first global key {plugin} bound in normal mode alone, unless the
    /// plugin manages modes itself: it bound a key in insert mode or holds
    /// normal mode. Such a binding was likely written when normal mode
    /// bindings also fired while typing, which they no longer do.
    pub fn normal_only_key(&self, plugin: &str) -> Option<Key> {
        let own = || self.bindings.iter().filter(|b| b.plugin.as_ref() == plugin);
        let manages_modes = self.normal_owner.as_deref() == Some(plugin)
            || own().any(|b| b.mode == InputMode::Insert);
        if manages_modes {
            return None;
        }
        own()
            .rev()
            .find(|b| b.mode == InputMode::Normal && b.is_global())
            .map(|b| b.key)
    }
}

/// The one gate for a key a plugin binds or claims, shared by
/// `maki.keymap.set` and a window's `keys` so a spelling that binds is exactly
/// one that can be claimed. Reserved keys are refused here, where the plugin
/// author sees the error, instead of becoming a binding that never fires.
pub(crate) fn accept_key(lhs: &str) -> LuaResult<Key> {
    let key = Key::parse(lhs).map_err(mlua::Error::runtime)?;
    if key.is_reserved() {
        return Err(mlua::Error::runtime(format!("{lhs} {RESERVED_KEY_ERR}")));
    }
    Ok(key)
}

fn publish_keymap_snapshot(lua: &Lua) {
    let Some(store) = lua.app_data_ref::<KeymapStore>() else {
        return;
    };
    let entries = store.snapshot_entries();
    drop(store);
    if let Some(writer) = lua.app_data_ref::<KeymapWriter>() {
        writer.publish(entries);
    }
}

fn apply_mode(lua: &Lua, mode: InputMode) {
    if let Some(writer) = lua.app_data_ref::<KeymapWriter>() {
        writer.mode.set(mode);
    }
    if let Some(waker) = lua.app_data_ref::<UiWaker>() {
        waker.wake();
    }
}

/// What `maki.ui.set_mode` does for {plugin}, which then owns normal mode
/// until anyone switches the mode again.
///
/// A handler can outlive its load, and [`release_mode`] already ran for it.
/// A normal mode it switched to then would be owned by nothing that could
/// ever switch it back, so the switch is dropped.
pub(crate) fn switch_mode(lua: &Lua, mode: InputMode, plugin: &Arc<str>) {
    if !plugin_loaded(lua, plugin) {
        tracing::debug!(mode = mode.letter(), plugin = %plugin, "mode switch dropped: plugin unloaded");
        return;
    }
    if let Some(mut store) = lua.app_data_mut::<KeymapStore>() {
        store.normal_owner = (mode == InputMode::Normal).then(|| Arc::clone(plugin));
    }
    apply_mode(lua, mode);
}

/// Puts the input back in insert mode if {plugin} left it in normal mode.
/// Once its load is gone, nothing is left to switch back and the chat input
/// could never be typed in again.
pub(crate) fn release_mode(lua: &Lua, plugin: &str) {
    let owned = lua.app_data_mut::<KeymapStore>().is_some_and(|mut store| {
        store
            .normal_owner
            .take_if(|owner| owner.as_ref() == plugin)
            .is_some()
    });
    if owned {
        apply_mode(lua, InputMode::Insert);
    }
}

fn store_mut(lua: &Lua) -> LuaResult<AppDataRefMut<'_, KeymapStore>> {
    lua.app_data_mut::<KeymapStore>()
        .ok_or_else(|| mlua::Error::runtime(NO_STORE_ERR))
}

fn parse_modes(mode: Value) -> LuaResult<Vec<InputMode>> {
    let modes = match mode {
        Value::String(letter) => vec![InputMode::parse(&letter.to_str()?)?],
        Value::Table(letters) => letters
            .sequence_values::<String>()
            .map(|letter| InputMode::parse(&letter?))
            .collect::<LuaResult<_>>()?,
        _ => return Err(mlua::Error::runtime(MODE_TYPE_ERR)),
    };
    if modes.is_empty() {
        return Err(mlua::Error::runtime(NO_MODES_ERR));
    }
    Ok(modes)
}

fn parse_buffer(opts: Option<&Table>) -> LuaResult<Scope> {
    let Some(opts) = opts else {
        return Ok(None);
    };
    match opts.get::<Value>("buffer")? {
        Value::Nil => Ok(None),
        Value::UserData(ud) => {
            let handle = ud
                .borrow::<BufHandle>()
                .map_err(|_| mlua::Error::runtime(BUFFER_TYPE_ERR))?;
            Ok(Some(Arc::downgrade(&handle.buf)))
        }
        _ => Err(mlua::Error::runtime(BUFFER_TYPE_ERR)),
    }
}

/// Bind a key to a Lua function, like `vim.keymap.set`.
///
/// The binding fires only in the [modes](/docs/lua-api/#modes) you list. A
/// key bound in `"i"` no longer types, so bind keys like `<Tab>` or `<C-x>`
/// there, rarely a letter. A binding on a key that types text, like `a`,
/// `<Space>` or `<Paste>`, needs `session_read` in every mode, because it
/// sees what the user types. A plugin may switch the mode itself, so a
/// letter bound in `"n"` could hear every key the user meant to type. That
/// goes for a buffer binding too, since its window can sit on screen while
/// the user types into the chat input. Keys
/// with `C-` or `M-`, function keys and keys like `<CR>`, `<Esc>`, `<Tab>`,
/// `<BS>` or the arrows need nothing.
/// [Key resolution](/docs/lua-api/#key-resolution) shows which binding gets
/// a key.
///
/// Bindings belong to the plugin that set them and stack per mode. The last
/// `set` wins and logs a warning naming the plugin it shadows. When that
/// plugin calls `del` or unloads, the shadowed binding comes back. Setting a
/// key you already hold replaces your binding.
///
/// A binding with `opts.buffer` comes before any global binding and goes
/// away with the buffer. Use it for the keys of a popup. A handler that
/// refers to the buffer keeps it alive, so `del` it when the popup closes.
///
/// The handler consumes the key, even when it throws (the error is logged).
/// Call `ev.fallback()` to let maki handle the key after all. A handler that
/// changes the text of the input needs `input`, see
/// [chat input](/docs/lua-api/#chat-input).
///
/// While a handler runs, the keys, pastes and clicks after it wait, so its
/// edits and mode switches are in place before the next key lands. Keys
/// wait one second at most, and stop waiting when another window takes the
/// keyboard, like a picker the handler opened. If a modal or a prompt opens
/// meanwhile, like an alert, a permission prompt or a question, the waiting
/// keys are dropped so they cannot answer it, and the status bar says so.
///
/// `<C-c>` and `<C-z>` never wait. Every other key waits its turn, `<Esc>`
/// while the agent runs included. A `<C-c>` while keys wait only drops them
/// and the pending `ev.fallback()`, and the status bar says so. The next
/// `<C-c>` works as usual. With no keys waiting, `<C-c>` drops the pending
/// `ev.fallback()` and works as usual.
///
/// Text that changed since a key was pressed is never sent by that key. If
/// anything changed the input while a `<CR>` waited, or the keyboard moved
/// meanwhile, the user never saw what it would send. The same goes for
/// `ev.fallback()` on `<CR>` once the input changed since the press, your
/// handler's own edits included. Maki drops that send and asks the user to
/// press Enter again.
///
/// A handler that changes the text of the input needs `input`, see
/// [chat input](/docs/lua-api/#chat-input).
///
/// Throws on an invalid mode or key, on `<C-c>` and `<C-z>`, which stay
/// reserved for quit and suspend, on a taken key with `unique`, and with
/// `permission denied` on a typing key in `"i"` without `session_read`.
///
/// @param mode string|string[] `"n"`, `"i"`, or a list of them.
/// @param lhs string Key in Vim notation, e.g. `"<C-t>"`, `"a"`, or `"<Paste>"` for bracketed paste.
/// @param rhs function Called with an event table `ev`. The return value is ignored. `ev` has:
///   `key` (string) the key, in canonical notation.
///   `input` (table?) the chat input at the press, as `maki.ui.input` returns it. Pass its `version` and `session_id` to `maki.ui.input_edit`. Only with `session_read`, else nil.
///   `text` (string?) `<Paste>` only: the pasted text, with `\n` line endings. maki does not insert it. It is cleaned the way `maki.ui.input_edit` cleans text, so `#ev.text` is what inserting it adds. Only with `session_read`, else nil.
///   `fallback` (function) runs maki's own handling of the key, as if no plugin had bound it (see [Key resolution](/docs/lua-api/#key-resolution)). Call it once, before the handler returns.
///     - It lands after the handler's earlier `maki.ui` calls and before the waiting keys.
///     - It returns `true` once the key is queued, or `nil` and an error.
///     - The key is dropped if the keyboard, the mode, the cursor or the selection moved since the press, your handler's own changes included.
/// @param opts table? Options:
///   `desc` (string) shown next to the key in `/help`.
///   `unique` (boolean) throw, naming the owner, when the key is already
///     bound in one of the modes. Default false.
///   `buffer` (Buf) bind the key only while a window showing this buffer is
///     on screen.
/// @example
/// -- Tab in an empty input starts a review prompt, else it switches plan/build:
/// maki.keymap.set("i", "<Tab>", function(ev)
///   local st = ev.input
///   if st.text ~= "" then
///     return ev.fallback()
///   end
///   maki.ui.input_edit({
///     start = 0, stop = 0, text = "Review the staged changes: ",
///     version = st.version, session_id = st.session_id,
///   })
/// end)
///
/// -- Keys of a popup, live only while it is on screen:
/// local buf = maki.ui.buf()
/// local win = maki.ui.open_win(buf, { focus = false, width = 20, height = 3 })
/// maki.keymap.set("i", "<Esc>", function()
///   maki.keymap.del("i", "<Esc>", { buffer = buf })
///   win:close()
/// end, { buffer = buf })
#[lua_fn]
fn set(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    #[ctx] grants: PluginPermissions,
    mode: Value,
    lhs: String,
    rhs: mlua::Function,
    opts: Option<Table>,
) -> LuaResult<()> {
    let modes = parse_modes(mode)?;
    let key = accept_key(&lhs)?;
    let buffer = parse_buffer(opts.as_ref())?;
    if key.types_text() {
        require(lua, &grants, Permission::SessionRead)?;
    }
    let opts = SetOpts {
        desc: opts
            .as_ref()
            .and_then(|o| o.get::<String>("desc").ok())
            .unwrap_or_default(),
        unique: opts
            .as_ref()
            .and_then(|o| o.opt_bool("unique"))
            .unwrap_or(false),
        buffer,
    };
    let registry_key = lua.create_registry_value(rhs)?;
    match store_mut(lua)?.set(key, &modes, registry_key, Arc::clone(&plugin), grants, opts) {
        SetOutcome::Free => {}
        SetOutcome::Shadowed(owner) => {
            tracing::warn!(key = %lhs, plugin = %plugin, shadowed = %owner, "keymap shadowed by plugin");
        }
        SetOutcome::Taken(owner) => {
            return Err(mlua::Error::runtime(format!("{lhs} {TAKEN_ERR} {owner}")));
        }
        SetOutcome::Stale => {
            tracing::debug!(key = %lhs, plugin = %plugin, "keymap dropped: plugin unloaded");
        }
    }
    publish_keymap_snapshot(lua);
    Ok(())
}

/// Remove your plugin's binding for {lhs} in {mode}, like `vim.keymap.del`.
/// The key goes back to the binding yours shadowed, or to maki's default.
///
/// Only your own bindings go. If another plugin holds {lhs}, nothing
/// changes and a warning names it. Only the modes you list are removed: a
/// binding set in `{ "n", "i" }` and removed in `"n"` stays in `"i"`.
/// Throws on an invalid mode or key.
///
/// @param mode string|string[] `"n"`, `"i"`, or a list of them.
/// @param lhs string Key to unmap, in Vim notation.
/// @param opts table? Options:
///   `buffer` (Buf) remove the binding set for this buffer instead of the
///     global one.
/// @example
/// maki.keymap.del({ "n", "i" }, "<C-t>")
#[lua_fn]
fn del(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    mode: Value,
    lhs: String,
    opts: Option<Table>,
) -> LuaResult<()> {
    let modes = parse_modes(mode)?;
    let key = Key::parse(&lhs).map_err(mlua::Error::runtime)?;
    let buffer = parse_buffer(opts.as_ref())?;
    let Some(mut store) = lua.app_data_mut::<KeymapStore>() else {
        return Ok(());
    };
    if !store.del(key, &modes, &plugin, &buffer) {
        if let Some(owner) = modes.iter().find_map(|&m| store.owner_of(key, m, &buffer)) {
            tracing::warn!(key = %lhs, plugin = %plugin, owner = %owner, "keymap del ignored: key belongs to another plugin");
        }
        return Ok(());
    }
    drop(store);
    publish_keymap_snapshot(lua);
    Ok(())
}

/// Canonical spelling of {lhs}. Accepts every spelling `set` accepts and
/// returns the string a `key` event carries.
///
/// @param lhs string Key in any accepted notation.
/// @return (string|nil, string|nil) Canonical notation, or nil and an error.
/// @example
/// local canon = maki.keymap.normalize("<Enter>")  -- "<CR>"
#[lua_fn]
fn normalize(_lua: &Lua, lhs: String) -> LuaResult<Pair<String>> {
    Ok(pair(Key::parse(&lhs).map(|key| key.notation())))
}

lua_table! {
    /// Key mappings, modeled after `vim.keymap`.
    ///
    /// ```lua
    /// maki.keymap.set({ "n", "i" }, "<C-t>", function()
    ///   print("hello")
    /// end, { desc = "Say hello" })
    /// ```
    ///
    /// ## Modes
    ///
    /// As in Neovim, maki is in one mode at a time, and a binding fires only
    /// in the modes it was set for.
    ///
    /// | Mode | When |
    /// | --- | --- |
    /// | `"i"` (insert, the default) | The chat input has the keyboard, and keys type into it. |
    /// | `"n"` (normal) | Set by `maki.ui.set_mode("n")`, and while a picker, modal, prompt or focused window has the keyboard or a finished subagent's chat is shown. |
    ///
    /// The mode a plugin set comes back when the chat input gets the keyboard
    /// again. In normal mode, keys never type or delete in the chat input.
    /// Moving the cursor, history, pasting and `<CR>` to send still work.
    ///
    /// Bind a shortcut in `{ "n", "i" }` to make it work everywhere, as the
    /// built-in plugins do. Read the mode with `maki.fn.mode()`, and watch it
    /// with the `"ModeChanged"` autocmd.
    ///
    /// ```lua
    /// -- A tiny vim mode: Esc leaves insert mode, i goes back.
    /// maki.keymap.set("i", "<Esc>", function() maki.ui.set_mode("n") end)
    /// maki.keymap.set("n", "i", function() maki.ui.set_mode("i") end)
    ///
    /// -- dd deletes the cursor's line. A lone d expires after 500 ms.
    /// local pending
    /// maki.keymap.set("n", "d", function(ev)
    ///   if not pending then
    ///     pending = maki.defer_fn(function() pending = nil end, 500)
    ///     return
    ///   end
    ///   pending:stop()
    ///   pending = nil
    ///   local st = ev.input
    ///   local head = st.text:sub(1, st.cursor):match("^.*\n")
    ///   maki.ui.input_edit({
    ///     start = head and #head or 0,
    ///     stop = st.text:find("\n", st.cursor + 1, true) or #st.text,
    ///     text = "", version = st.version, session_id = st.session_id,
    ///   })
    /// end)
    /// ```
    ///
    /// With this `<Esc>` binding, the built-in `Esc Esc` (rewind) works only
    /// from normal mode.
    ///
    /// Older versions fired `"n"` bindings while you typed. See
    /// [Upgrading old keymaps](/docs/keybindings/#upgrading-old-keymaps).
    ///
    /// ## Key resolution
    ///
    /// Each key runs one handler: the first match in this list, for the
    /// current mode.
    ///
    /// 1. `<C-c>` and `<C-z>`, which no binding can take. `<C-z>` suspends
    ///    here, and `<C-c>` still goes to step 2 first.
    /// 2. Maki's modals and prompts, then the focused window, then Maki's
    ///    pickers. They take every key, except the plan form and the `/`
    ///    command list, which pass on the keys they do not use. The focused
    ///    window runs its buffer's bindings, then its `win:recv`.
    /// 3. Windows on screen, front first: each one's buffer bindings, then
    ///    the keys it claimed with the `keys` option of `maki.ui.open_win`.
    /// 4. `<Esc>` while the agent runs, in either mode. Maki takes it, and a
    ///    second one in a row cancels the turn.
    /// 5. Global bindings, the last `set` first.
    /// 6. Maki's built-in keys and the chat input.
    ///
    /// `ev.fallback()` jumps to step 6, or to `win:recv` for a binding on the
    /// focused window's buffer. `<Paste>` follows the same list.
    ///
    /// To share one key between plugins, see
    /// [Composing keys](/docs/keybindings/#composing-keys).
    ///
    /// ## Key notation
    ///
    /// `set`, `del`, the `keys` option of `maki.ui.open_win` and `win:recv`
    /// key events all use one notation. `normalize` converts any accepted
    /// spelling to the canonical one.
    ///
    /// ```lua
    /// if ev.type == "key" and ev.key == "<CR>" then submit() end
    /// ```
    ///
    /// A single character stands for itself: `a`, `A`, `7`, `?`. Other keys
    /// go in angle brackets, after any modifiers.
    ///
    /// | Key | Notation | Also accepted |
    /// | --- | --- | --- |
    /// | Enter | `<CR>` | `<Enter>`, `<Return>` |
    /// | Escape | `<Esc>` | `<Escape>` |
    /// | Backspace | `<BS>` | `<Backspace>` |
    /// | Delete | `<Del>` | `<Delete>` |
    /// | Tab | `<Tab>` | |
    /// | Shift+Tab | `<S-Tab>` | |
    /// | Space | `<Space>` | |
    /// | Arrows | `<Up>`, `<Down>`, `<Left>`, `<Right>` | |
    /// | Navigation | `<Home>`, `<End>`, `<PageUp>`, `<PageDown>`, `<Insert>` | |
    /// | Function keys | `<F1>` through `<F24>` | |
    /// | Bracketed paste | `<Paste>` | |
    ///
    /// Modifiers are `C-` (control), `M-` (alt) and `S-` (shift), in that
    /// order: `<C-M-x>`. `Ctrl-`, `Alt-`, `A-` and `Shift-` are accepted as
    /// input.
    ///
    /// Terminals report some keys differently, so maki picks one form:
    ///
    /// - Control plus a letter is lowercase: `<C-N>` is `<C-n>`, as in Vim.
    /// - Shift plus a letter is the uppercase letter: `<S-a>` is `A`.
    /// - Without control or alt, shift is part of the char typed, so the key
    ///   is that char: Shift+1 on a US layout is `!`, and `<S-!>` is `!`.
    ///   `<S-Space>` is `<Space>`. With alt the prefix stays: `<M-S-1>`.
    /// - Shift+Tab is always `<S-Tab>`, with or without the kitty keyboard
    ///   protocol.
    ///
    /// Key strings in a plugin and every module it `require`s are checked at
    /// load. Each invalid one is logged with its file and line, and the status
    /// bar shows a summary, so a typo shows up at startup.
    ///
    /// Upgrading from older versions: `win:recv` used to deliver `"enter"`,
    /// `"esc"`, `"ctrl+n"` and `"shift+tab"`. These now arrive as `<CR>`,
    /// `<Esc>`, `<C-n>` and `<S-Tab>`, and the load check flags the old
    /// spellings.
    "maki.keymap" => pub(crate) fn create_keymap_table(plugin: Arc<str>, grants: PluginPermissions), DOCS [
        set(plugin, grants), del(plugin), normalize,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const PLUGIN: &str = "plug";
    const OTHER_PLUGIN: &str = "other";
    const TAB: &str = "<Tab>";
    const MODE: InputMode = InputMode::Normal;
    const BOTH: &[InputMode] = &[InputMode::Normal, InputMode::Insert];

    fn parsed(lhs: &str) -> Key {
        Key::parse(lhs).unwrap()
    }

    fn global(store: &mut KeymapStore, lua: &Lua, lhs: &str, plugin: &str) -> SetOutcome {
        bind(store, lua, lhs, plugin, &[MODE], false)
    }

    fn bind(
        store: &mut KeymapStore,
        lua: &Lua,
        lhs: &str,
        plugin: &str,
        modes: &[InputMode],
        unique: bool,
    ) -> SetOutcome {
        let opts = SetOpts {
            unique,
            ..SetOpts::default()
        };
        set_with(store, lua, lhs, plugin, modes, opts)
    }

    fn bind_on(
        store: &mut KeymapStore,
        lua: &Lua,
        plugin: &str,
        buf: &Arc<SharedBuf>,
    ) -> SetOutcome {
        let opts = SetOpts {
            buffer: Some(Arc::downgrade(buf)),
            ..SetOpts::default()
        };
        set_with(store, lua, TAB, plugin, &[MODE], opts)
    }

    fn set_with(
        store: &mut KeymapStore,
        lua: &Lua,
        lhs: &str,
        plugin: &str,
        modes: &[InputMode],
        opts: SetOpts,
    ) -> SetOutcome {
        let f = lua.create_function(|_, ()| Ok(())).unwrap();
        let k = lua.create_registry_value(f).unwrap();
        store.set(
            parsed(lhs),
            modes,
            k,
            Arc::from(plugin),
            PluginPermissions::trusted(),
            opts,
        )
    }

    fn published(store: &KeymapStore) -> KeymapReader {
        let (writer, reader) = KeymapWriter::new();
        writer.publish(store.snapshot_entries());
        reader
    }

    /// Which plugin's binding for {lhs} fires.
    fn dispatching_owner(store: &KeymapStore, lhs: &str) -> Option<Arc<str>> {
        owner_on(store, lhs, None)
    }

    fn owner_on(
        store: &KeymapStore,
        lhs: &str,
        buffer: Option<&Arc<SharedBuf>>,
    ) -> Option<Arc<str>> {
        let mut owner = None;
        published(store).dispatch(parsed(lhs), MODE, buffer, |ticket| {
            owner = Some(Arc::clone(ticket.plugin()));
            true
        });
        owner
    }

    /// Two plugins wanting one key is the ordinary case, and the one underneath
    /// has to get its key back rather than lose it for the rest of the run.
    #[test]
    fn a_shadowed_binding_comes_back_when_the_one_over_it_goes_away() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        global(&mut store, &lua, TAB, PLUGIN);
        global(&mut store, &lua, TAB, OTHER_PLUGIN);
        assert_eq!(
            dispatching_owner(&store, TAB).as_deref(),
            Some(OTHER_PLUGIN),
            "the newest binding is the one that fires"
        );

        store.del(parsed(TAB), &[MODE], OTHER_PLUGIN, &None);
        assert_eq!(
            dispatching_owner(&store, TAB).as_deref(),
            Some(PLUGIN),
            "removing it uncovers the binding underneath"
        );

        global(&mut store, &lua, TAB, OTHER_PLUGIN);
        store.clear_plugin(OTHER_PLUGIN);
        assert_eq!(
            dispatching_owner(&store, TAB).as_deref(),
            Some(PLUGIN),
            "unloading the owner uncovers it the same way"
        );
    }

    /// Rebinding your own key replaces your entry. Stacking it on yourself
    /// would take one `del` per `set` to hand the key back.
    #[test]
    fn setting_your_own_key_again_replaces_it_rather_than_stacking() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, OTHER_PLUGIN);
        global(&mut store, &lua, TAB, PLUGIN);
        global(&mut store, &lua, TAB, PLUGIN);

        store.del(parsed(TAB), &[MODE], PLUGIN, &None);

        assert_eq!(
            dispatching_owner(&store, TAB).as_deref(),
            Some(OTHER_PLUGIN)
        );
    }

    /// Cleaning up your own binding must not take a key someone else stacked
    /// on top of it.
    #[test]
    fn del_by_an_owner_underneath_leaves_the_binding_on_top() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);
        global(&mut store, &lua, TAB, OTHER_PLUGIN);

        assert!(store.del(parsed(TAB), &[MODE], PLUGIN, &None));

        assert_eq!(
            dispatching_owner(&store, TAB).as_deref(),
            Some(OTHER_PLUGIN)
        );
        store.del(parsed(TAB), &[MODE], OTHER_PLUGIN, &None);
        assert_eq!(
            dispatching_owner(&store, TAB),
            None,
            "the owner's binding is gone, not buried"
        );
    }

    #[test]
    fn keymap_store_clear_plugin() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        global(&mut store, &lua, "t", PLUGIN);
        global(&mut store, &lua, "x", OTHER_PLUGIN);

        store.clear_plugin(PLUGIN);
        assert_eq!(store.bindings.len(), 1);
        assert_eq!(store.bindings[0].plugin.as_ref(), OTHER_PLUGIN);
    }

    #[test]
    fn snapshot_reader_writer() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        let (writer, reader) = KeymapWriter::new();
        assert!(reader.load().entries.is_empty());

        global(&mut store, &lua, TAB, PLUGIN);
        writer.publish(store.snapshot_entries());

        let snap = reader.load();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.generation, 1);
    }

    /// The key is consumed the moment `dispatch` says it was claimed, so a
    /// hand-off that did not happen has to report so and let the built-in
    /// binding run.
    #[test_case(true  => true  ; "claimed_when_the_callback_was_reached")]
    #[test_case(false => false ; "falls_through_when_it_was_not")]
    fn dispatch_reports_whether_the_key_was_claimed(handed_off: bool) -> bool {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);

        published(&store).dispatch(parsed(TAB), MODE, None, |_| handed_off)
    }

    #[test]
    fn dispatch_leaves_a_key_nobody_claimed_alone() {
        let store = KeymapStore::new();
        assert!(!published(&store).dispatch(parsed(TAB), MODE, None, |_| unreachable!()));
    }

    /// A keystroke claimed a moment before a `/reload` carries the old chunk's
    /// callback with it. Its plugin is gone, so the host has to be told here,
    /// while it can still run the built-in binding for the key.
    #[test]
    fn a_binding_whose_plugin_was_torn_down_claims_nothing() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);
        let reader = published(&store);

        let mut live = None;
        assert!(reader.dispatch(parsed(TAB), MODE, None, |t| {
            live = Some(t.plugin_live());
            true
        }));
        assert_eq!(live, Some(true));

        store.clear_plugin(PLUGIN);
        assert!(
            !reader.dispatch(parsed(TAB), MODE, None, |_| unreachable!()),
            "the stale snapshot no longer claims the key"
        );
    }

    /// A ticket outlives the snapshot it came from, so the answer has to travel
    /// with it: a `/reload` between the claim and the call must reach the
    /// callback that is about to run.
    #[test]
    fn a_ticket_reports_the_teardown_that_landed_after_it_was_claimed() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);

        let mut held = None;
        published(&store).dispatch(parsed(TAB), MODE, None, |t| {
            held = Some(t);
            true
        });
        let ticket = held.unwrap();
        assert!(ticket.plugin_live());
        assert_eq!(ticket.plugin().as_ref(), PLUGIN);

        store.clear_plugin(PLUGIN);
        assert!(!ticket.plugin_live());
    }

    /// The handler runs on another thread, so the key the host consumed travels
    /// with the ticket: it is what the log names when the callback cannot be
    /// reached at all.
    #[test]
    fn a_ticket_carries_the_key_it_was_claimed_for() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);

        let mut carried = None;
        published(&store).dispatch(parsed(TAB), MODE, None, |t| {
            carried = Some(t.key());
            true
        });
        assert_eq!(carried, Some(parsed(TAB)));
    }

    /// A reserved key binds nowhere. Accepting it publishes a binding with a
    /// `desc` in the keymap list that can never fire, which is a bug report
    /// about maki rather than about the plugin that wrote it.
    #[test_case("<C-c>" ; "quit")]
    #[test_case("<C-z>" ; "suspend")]
    fn a_reserved_key_is_refused_where_the_author_can_see_it(lhs: &str) {
        let err = accept_key(lhs).unwrap_err().to_string();
        assert!(err.contains(RESERVED_KEY_ERR), "got: {err}");
        assert!(
            err.contains(lhs),
            "the error has to name the key, got: {err}"
        );
    }

    /// A tombstone, not a removal: a handler of the load that is gone can
    /// still be running when `/reload` lands, and the bindings it publishes on
    /// its way out belong to nothing. The load that replaces it starts live.
    #[test]
    fn a_torn_down_plugin_cannot_publish_live_bindings_until_it_is_reloaded() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);

        store.clear_plugin(PLUGIN);
        global(&mut store, &lua, TAB, PLUGIN);
        assert!(
            !published(&store).dispatch(parsed(TAB), MODE, None, |_| unreachable!()),
            "a straggler of the torn down load publishes nothing that fires"
        );

        store.revive(PLUGIN);
        global(&mut store, &lua, TAB, PLUGIN);
        assert!(
            published(&store).dispatch(parsed(TAB), MODE, None, |_| true),
            "the load that replaced it dispatches"
        );
    }

    /// The straggler's `set` stores nothing, which is what keeps a live
    /// binding from another plugin from being buried under a dead one until
    /// the next reload.
    #[test]
    fn a_straggler_does_not_bury_a_live_binding_under_a_dead_one() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);
        store.clear_plugin(PLUGIN);
        global(&mut store, &lua, TAB, OTHER_PLUGIN);

        assert!(matches!(
            global(&mut store, &lua, TAB, PLUGIN),
            SetOutcome::Stale
        ));
        assert_eq!(
            dispatching_owner(&store, TAB).as_deref(),
            Some(OTHER_PLUGIN),
            "the live binding still fires"
        );
    }

    /// A letter bound in normal mode has to leave typing alone, and a
    /// shortcut bound in both modes has to work in either.
    #[test_case(&[InputMode::Normal], InputMode::Insert => false ; "normal_rests_while_typing")]
    #[test_case(&[InputMode::Insert], InputMode::Normal => false ; "insert_rests_in_normal")]
    #[test_case(BOTH,                 InputMode::Insert => true  ; "both_fires_while_typing")]
    #[test_case(BOTH,                 InputMode::Normal => true  ; "both_fires_in_normal")]
    fn a_binding_fires_only_in_its_modes(modes: &[InputMode], mode: InputMode) -> bool {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        bind(&mut store, &lua, TAB, PLUGIN, modes, false);
        published(&store).dispatch(parsed(TAB), mode, None, |_| true)
    }

    #[test]
    fn del_in_one_mode_leaves_the_other_bound() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        bind(&mut store, &lua, TAB, PLUGIN, BOTH, false);

        assert!(store.del(parsed(TAB), &[InputMode::Normal], PLUGIN, &None));

        let reader = published(&store);
        assert!(!reader.dispatch(parsed(TAB), InputMode::Normal, None, |_| unreachable!()));
        assert!(reader.dispatch(parsed(TAB), InputMode::Insert, None, |_| true));
    }

    /// Half a binding is worse than none. The author asked for the key in
    /// both modes and would only find out by pressing it in the wrong one.
    #[test]
    fn a_unique_bind_taken_in_one_mode_stores_nothing() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        bind(
            &mut store,
            &lua,
            TAB,
            OTHER_PLUGIN,
            &[InputMode::Insert],
            false,
        );

        let outcome = bind(&mut store, &lua, TAB, PLUGIN, BOTH, true);

        assert!(matches!(outcome, SetOutcome::Taken(owner) if owner.as_ref() == OTHER_PLUGIN));
        assert_eq!(
            dispatching_owner(&store, TAB),
            None,
            "normal mode stayed free"
        );
    }

    /// The host asks a buffer's bindings only for the windows showing it, and
    /// at another step than the global ones. So a popup's key fires nowhere
    /// else, and neither scope can hide the other.
    #[test]
    fn buffer_and_global_bindings_never_mix() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        let (mine, other) = (Arc::new(SharedBuf::new()), Arc::new(SharedBuf::new()));
        global(&mut store, &lua, TAB, OTHER_PLUGIN);

        let outcome = bind_on(&mut store, &lua, PLUGIN, &mine);
        assert!(matches!(outcome, SetOutcome::Free), "no shadowing warning");
        assert_eq!(owner_on(&store, TAB, Some(&mine)).as_deref(), Some(PLUGIN));
        assert_eq!(owner_on(&store, TAB, Some(&other)), None);
        assert_eq!(
            dispatching_owner(&store, TAB).as_deref(),
            Some(OTHER_PLUGIN)
        );

        assert!(store.del(parsed(TAB), &[MODE], PLUGIN, &Some(Arc::downgrade(&mine))));
        assert_eq!(owner_on(&store, TAB, Some(&mine)), None);
        assert_eq!(
            dispatching_owner(&store, TAB).as_deref(),
            Some(OTHER_PLUGIN),
            "the del left the global binding alone"
        );
    }

    /// No window can show a buffer that is gone, and each callback left behind
    /// still holds a registry slot.
    #[test]
    fn the_bindings_of_a_buffer_that_is_gone_are_dropped() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        let buf = Arc::new(SharedBuf::new());
        bind_on(&mut store, &lua, PLUGIN, &buf);

        drop(buf);
        global(&mut store, &lua, TAB, PLUGIN);

        assert_eq!(store.bindings.len(), 1, "only the global binding is left");
    }

    #[test_case(r#""i""#,          Ok(&[InputMode::Insert]) ; "one_letter")]
    #[test_case(r#"{ "n", "i" }"#, Ok(BOTH)                 ; "a_list")]
    #[test_case(r#""v""#,          Err(UNKNOWN_MODE_ERR)    ; "an_unknown_letter")]
    #[test_case("{}",              Err(NO_MODES_ERR)        ; "an_empty_list")]
    fn modes_parse(src: &str, expected: Result<&[InputMode], &str>) {
        let lua = Lua::new();
        let parsed = parse_modes(lua.load(src).eval().unwrap()).map_err(|e| e.to_string());
        match expected {
            Ok(modes) => assert_eq!(parsed.unwrap(), modes),
            Err(msg) => assert!(parsed.unwrap_err().contains(msg)),
        }
    }

    /// `maki.fn.mode()` reads the Lua half and keys are matched with the UI
    /// half, so both have to agree, and a plugin's pick only counts while
    /// the chat input has the keyboard.
    #[test_case(InputMode::Insert, true  => InputMode::Insert ; "typing")]
    #[test_case(InputMode::Normal, true  => InputMode::Normal ; "a_plugin_picked_normal")]
    #[test_case(InputMode::Insert, false => InputMode::Normal ; "the_input_lost_the_keyboard")]
    fn the_mode_is_insert_only_while_the_input_has_the_keyboard(
        picked: InputMode,
        focused: bool,
    ) -> InputMode {
        let (writer, reader) = KeymapWriter::new();
        writer.mode.set(picked);
        let mode = reader.sync_mode(focused);
        assert_eq!(writer.mode.get(), mode);
        mode
    }
}
