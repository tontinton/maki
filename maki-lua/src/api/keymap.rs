use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use crossterm::event::{KeyCode, KeyModifiers};
use mlua::{Lua, RegistryKey, Result as LuaResult, Table};

static NEXT_KEYMAP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct KeymapEntry {
    pub key: KeyCode,
    pub modifiers: KeyModifiers,
    pub desc: String,
    pub plugin: Arc<str>,
    pub id: u64,
}

#[derive(Clone, Default)]
pub struct KeymapSnapshot {
    pub entries: Vec<KeymapEntry>,
    pub generation: u64,
}

#[derive(Clone)]
pub struct KeymapReader(Arc<ArcSwap<KeymapSnapshot>>);

impl KeymapReader {
    pub fn empty() -> Self {
        Self(Arc::new(ArcSwap::from_pointee(KeymapSnapshot::default())))
    }

    pub fn load(&self) -> arc_swap::Guard<Arc<KeymapSnapshot>> {
        self.0.load()
    }
}

pub(crate) struct KeymapWriter {
    store: Arc<ArcSwap<KeymapSnapshot>>,
    generation: AtomicU64,
}

impl KeymapWriter {
    pub fn new() -> (Self, KeymapReader) {
        let inner = Arc::new(ArcSwap::from_pointee(KeymapSnapshot::default()));
        (
            Self {
                store: Arc::clone(&inner),
                generation: AtomicU64::new(0),
            },
            KeymapReader(inner),
        )
    }

    pub fn publish(&self, entries: Vec<KeymapEntry>) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.store.store(Arc::new(KeymapSnapshot {
            entries,
            generation,
        }));
    }
}

pub(crate) struct StoredKeymap {
    pub id: u64,
    pub key: KeyCode,
    pub modifiers: KeyModifiers,
    pub callback: RegistryKey,
    pub plugin: Arc<str>,
    pub desc: String,
}

pub(crate) struct KeymapStore {
    bindings: Vec<StoredKeymap>,
}

impl KeymapStore {
    pub fn new() -> Self {
        Self {
            bindings: Vec::new(),
        }
    }

    pub fn set(
        &mut self,
        key: KeyCode,
        modifiers: KeyModifiers,
        callback: RegistryKey,
        plugin: Arc<str>,
        desc: String,
    ) -> (u64, Option<RegistryKey>) {
        let id = NEXT_KEYMAP_ID.fetch_add(1, Ordering::Relaxed);
        let old = self
            .bindings
            .iter()
            .position(|b| b.key == key && b.modifiers == modifiers)
            .map(|pos| self.bindings.remove(pos).callback);
        self.bindings.push(StoredKeymap {
            id,
            key,
            modifiers,
            callback,
            plugin,
            desc,
        });
        (id, old)
    }

    pub fn del(&mut self, key: KeyCode, modifiers: KeyModifiers) -> Option<RegistryKey> {
        self.bindings
            .iter()
            .position(|b| b.key == key && b.modifiers == modifiers)
            .map(|pos| self.bindings.remove(pos).callback)
    }

    pub fn clear_plugin(&mut self, plugin: &str) -> Vec<RegistryKey> {
        let mut keys = Vec::new();
        let mut i = 0;
        while i < self.bindings.len() {
            if self.bindings[i].plugin.as_ref() == plugin {
                keys.push(self.bindings.remove(i).callback);
            } else {
                i += 1;
            }
        }
        keys
    }

    pub fn snapshot_entries(&self) -> Vec<KeymapEntry> {
        self.bindings
            .iter()
            .map(|b| KeymapEntry {
                key: b.key,
                modifiers: b.modifiers,
                desc: b.desc.clone(),
                plugin: Arc::clone(&b.plugin),
                id: b.id,
            })
            .collect()
    }

    pub fn callback_for_id(&self, id: u64) -> Option<&RegistryKey> {
        self.bindings
            .iter()
            .find(|b| b.id == id)
            .map(|b| &b.callback)
    }
}

pub fn parse_key_notation(input: &str) -> Result<(KeyCode, KeyModifiers), String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty key notation".into());
    }

    if s.starts_with('<') && s.ends_with('>') {
        let inner = &s[1..s.len() - 1];
        return parse_bracketed(inner);
    }

    if s.len() == 1 {
        let c = s.chars().next().unwrap();
        return Ok((KeyCode::Char(c), KeyModifiers::NONE));
    }

    Err(format!("invalid key notation: {s}"))
}

fn parse_bracketed(inner: &str) -> Result<(KeyCode, KeyModifiers), String> {
    if inner.is_empty() {
        return Err("empty angle-bracket key notation".into());
    }

    let mut modifiers = KeyModifiers::NONE;
    let mut rest = inner;

    loop {
        let lower = rest.to_lowercase();
        if lower.starts_with("c-") {
            modifiers |= KeyModifiers::CONTROL;
            rest = &rest[2..];
        } else if lower.starts_with("ctrl-") {
            modifiers |= KeyModifiers::CONTROL;
            rest = &rest[5..];
        } else if lower.starts_with("a-") {
            modifiers |= KeyModifiers::ALT;
            rest = &rest[2..];
        } else if lower.starts_with("alt-") {
            modifiers |= KeyModifiers::ALT;
            rest = &rest[4..];
        } else if lower.starts_with("m-") {
            modifiers |= KeyModifiers::ALT;
            rest = &rest[2..];
        } else if lower.starts_with("s-") {
            modifiers |= KeyModifiers::SHIFT;
            rest = &rest[2..];
        } else if lower.starts_with("shift-") {
            modifiers |= KeyModifiers::SHIFT;
            rest = &rest[6..];
        } else {
            break;
        }
    }

    let key = parse_key_name(rest)?;
    Ok((key, modifiers))
}

fn parse_key_name(name: &str) -> Result<KeyCode, String> {
    let lower = name.to_lowercase();
    match lower.as_str() {
        "cr" | "enter" | "return" => Ok(KeyCode::Enter),
        "space" => Ok(KeyCode::Char(' ')),
        "esc" | "escape" => Ok(KeyCode::Esc),
        "tab" => Ok(KeyCode::Tab),
        "bs" | "backspace" => Ok(KeyCode::Backspace),
        "del" | "delete" => Ok(KeyCode::Delete),
        "up" => Ok(KeyCode::Up),
        "down" => Ok(KeyCode::Down),
        "left" => Ok(KeyCode::Left),
        "right" => Ok(KeyCode::Right),
        "home" => Ok(KeyCode::Home),
        "end" => Ok(KeyCode::End),
        "pageup" => Ok(KeyCode::PageUp),
        "pagedown" => Ok(KeyCode::PageDown),
        "insert" => Ok(KeyCode::Insert),
        s if s.starts_with('f') && s.len() > 1 => {
            let n: u8 = s[1..]
                .parse()
                .map_err(|_| format!("invalid function key: {name}"))?;
            if !(1..=12).contains(&n) {
                return Err(format!("function key out of range: {name}"));
            }
            Ok(KeyCode::F(n))
        }
        _ => {
            if name.len() == 1 {
                Ok(KeyCode::Char(name.chars().next().unwrap()))
            } else {
                Err(format!("unknown key: {name}"))
            }
        }
    }
}

pub(crate) fn create_keymap_table(lua: &Lua, plugin: Arc<str>) -> LuaResult<Table> {
    let t = lua.create_table()?;

    let p = Arc::clone(&plugin);
    t.set(
        "set",
        lua.create_function(
            move |lua,
                  (mode, key_str, callback, opts): (
                String,
                String,
                mlua::Function,
                Option<Table>,
            )| {
                if mode != "n" {
                    return Err(mlua::Error::runtime(format!(
                        "unsupported keymap mode: {mode}"
                    )));
                }
                let (key, modifiers) =
                    parse_key_notation(&key_str).map_err(mlua::Error::runtime)?;
                let desc = opts
                    .as_ref()
                    .and_then(|o| o.get::<String>("desc").ok())
                    .unwrap_or_default();
                let registry_key = lua.create_registry_value(callback)?;
                let (_, old) = lua
                    .app_data_mut::<KeymapStore>()
                    .ok_or_else(|| mlua::Error::runtime("keymap store not initialized"))?
                    .set(key, modifiers, registry_key, Arc::clone(&p), desc);
                if let Some(old_key) = old {
                    tracing::warn!(key = %key_str, plugin = %p, "keymap shadowed by plugin");
                    let _ = lua.remove_registry_value(old_key);
                }
                publish_keymap_snapshot(lua);
                Ok(())
            },
        )?,
    )?;

    t.set(
        "del",
        lua.create_function(|lua, (_mode, key_str): (String, String)| {
            let (key, modifiers) = parse_key_notation(&key_str).map_err(mlua::Error::runtime)?;
            let old = lua
                .app_data_mut::<KeymapStore>()
                .and_then(|mut store| store.del(key, modifiers));
            if let Some(old_key) = old {
                let _ = lua.remove_registry_value(old_key);
            }
            publish_keymap_snapshot(lua);
            Ok(())
        })?,
    )?;

    Ok(t)
}

fn publish_keymap_snapshot(lua: &Lua) {
    if let Some(store) = lua.app_data_ref::<KeymapStore>() {
        let entries = store.snapshot_entries();
        if let Some(writer) = lua.app_data_ref::<KeymapWriter>() {
            writer.publish(entries);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use test_case::test_case;

    #[test_case("<C-t>", KeyCode::Char('t'), KeyModifiers::CONTROL ; "ctrl_t")]
    #[test_case("<C-T>", KeyCode::Char('T'), KeyModifiers::CONTROL ; "ctrl_shift_t")]
    #[test_case("<A-x>", KeyCode::Char('x'), KeyModifiers::ALT ; "alt_x")]
    #[test_case("<M-x>", KeyCode::Char('x'), KeyModifiers::ALT ; "meta_x")]
    #[test_case("<S-Tab>", KeyCode::Tab, KeyModifiers::SHIFT ; "shift_tab")]
    #[test_case("<CR>", KeyCode::Enter, KeyModifiers::NONE ; "enter_cr")]
    #[test_case("<Enter>", KeyCode::Enter, KeyModifiers::NONE ; "enter_full")]
    #[test_case("<Space>", KeyCode::Char(' '), KeyModifiers::NONE ; "space")]
    #[test_case("<Esc>", KeyCode::Esc, KeyModifiers::NONE ; "escape")]
    #[test_case("<Tab>", KeyCode::Tab, KeyModifiers::NONE ; "tab")]
    #[test_case("<BS>", KeyCode::Backspace, KeyModifiers::NONE ; "backspace_short")]
    #[test_case("<Backspace>", KeyCode::Backspace, KeyModifiers::NONE ; "backspace_full")]
    #[test_case("<Del>", KeyCode::Delete, KeyModifiers::NONE ; "delete_short")]
    #[test_case("<Delete>", KeyCode::Delete, KeyModifiers::NONE ; "delete_full")]
    #[test_case("<Up>", KeyCode::Up, KeyModifiers::NONE ; "up")]
    #[test_case("<Down>", KeyCode::Down, KeyModifiers::NONE ; "down")]
    #[test_case("<Left>", KeyCode::Left, KeyModifiers::NONE ; "left")]
    #[test_case("<Right>", KeyCode::Right, KeyModifiers::NONE ; "right")]
    #[test_case("<Home>", KeyCode::Home, KeyModifiers::NONE ; "home")]
    #[test_case("<End>", KeyCode::End, KeyModifiers::NONE ; "end_key")]
    #[test_case("<PageUp>", KeyCode::PageUp, KeyModifiers::NONE ; "page_up")]
    #[test_case("<PageDown>", KeyCode::PageDown, KeyModifiers::NONE ; "page_down")]
    #[test_case("<Insert>", KeyCode::Insert, KeyModifiers::NONE ; "insert")]
    #[test_case("<F1>", KeyCode::F(1), KeyModifiers::NONE ; "f1")]
    #[test_case("<F12>", KeyCode::F(12), KeyModifiers::NONE ; "f12")]
    #[test_case("a", KeyCode::Char('a'), KeyModifiers::NONE ; "plain_a")]
    #[test_case("z", KeyCode::Char('z'), KeyModifiers::NONE ; "plain_z")]
    #[test_case("<C-S-a>", KeyCode::Char('a'), KeyModifiers::from_bits_truncate(KeyModifiers::CONTROL.bits() | KeyModifiers::SHIFT.bits()) ; "ctrl_shift_a")]
    #[test_case("<Ctrl-x>", KeyCode::Char('x'), KeyModifiers::CONTROL ; "ctrl_long_x")]
    #[test_case("<Alt-j>", KeyCode::Char('j'), KeyModifiers::ALT ; "alt_long_j")]
    #[test_case("<Shift-Tab>", KeyCode::Tab, KeyModifiers::SHIFT ; "shift_long_tab")]
    #[test_case("<Return>", KeyCode::Enter, KeyModifiers::NONE ; "return_key")]
    #[test_case("<Escape>", KeyCode::Esc, KeyModifiers::NONE ; "escape_full")]
    fn parse_key_notation_cases(input: &str, code: KeyCode, mods: KeyModifiers) {
        let (key, modifiers) = parse_key_notation(input).unwrap();
        assert_eq!(key, code);
        assert_eq!(modifiers, mods);
    }

    #[test]
    fn parse_key_notation_errors() {
        assert!(parse_key_notation("").is_err());
        assert!(parse_key_notation("<>").is_err());
        assert!(parse_key_notation("<F0>").is_err());
        assert!(parse_key_notation("<F13>").is_err());
        assert!(parse_key_notation("abc").is_err());
    }

    #[test]
    fn keymap_store_set_and_shadow() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        let f1 = lua.create_function(|_, ()| Ok(())).unwrap();
        let k1 = lua.create_registry_value(f1).unwrap();
        let (id1, old1) = store.set(
            KeyCode::Char('t'),
            KeyModifiers::CONTROL,
            k1,
            Arc::from("plug"),
            "toggle".into(),
        );
        assert!(old1.is_none());

        let f2 = lua.create_function(|_, ()| Ok(())).unwrap();
        let k2 = lua.create_registry_value(f2).unwrap();
        let (id2, old2) = store.set(
            KeyCode::Char('t'),
            KeyModifiers::CONTROL,
            k2,
            Arc::from("plug2"),
            "toggle v2".into(),
        );
        assert!(old2.is_some());
        assert_ne!(id1, id2);
        assert_eq!(store.bindings.len(), 1);
    }

    #[test]
    fn keymap_store_del() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        let f = lua.create_function(|_, ()| Ok(())).unwrap();
        let k = lua.create_registry_value(f).unwrap();
        store.set(
            KeyCode::Char('x'),
            KeyModifiers::ALT,
            k,
            Arc::from("p"),
            String::new(),
        );
        assert_eq!(store.bindings.len(), 1);

        let removed = store.del(KeyCode::Char('x'), KeyModifiers::ALT);
        assert!(removed.is_some());
        assert!(store.bindings.is_empty());

        let missing = store.del(KeyCode::Char('x'), KeyModifiers::ALT);
        assert!(missing.is_none());
    }

    #[test]
    fn keymap_store_clear_plugin() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        let f1 = lua.create_function(|_, ()| Ok(())).unwrap();
        let f2 = lua.create_function(|_, ()| Ok(())).unwrap();
        let k1 = lua.create_registry_value(f1).unwrap();
        let k2 = lua.create_registry_value(f2).unwrap();
        store.set(
            KeyCode::Char('t'),
            KeyModifiers::CONTROL,
            k1,
            Arc::from("a"),
            String::new(),
        );
        store.set(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
            k2,
            Arc::from("b"),
            String::new(),
        );

        let removed = store.clear_plugin("a");
        assert_eq!(removed.len(), 1);
        assert_eq!(store.bindings.len(), 1);
        assert_eq!(store.bindings[0].plugin.as_ref(), "b");
    }

    #[test]
    fn snapshot_reader_writer() {
        let (writer, reader) = KeymapWriter::new();
        assert!(reader.load().entries.is_empty());

        writer.publish(vec![KeymapEntry {
            key: KeyCode::Char('t'),
            modifiers: KeyModifiers::CONTROL,
            desc: "test".into(),
            plugin: Arc::from("p"),
            id: 1,
        }]);

        let snap = reader.load();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.generation, 1);
    }
}
