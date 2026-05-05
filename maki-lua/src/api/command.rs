use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use mlua::RegistryKey;

#[derive(Clone)]
pub struct LuaCommandInfo {
    pub name: Arc<str>,
    pub description: Arc<str>,
    pub plugin: Arc<str>,
}

#[derive(Clone, Default)]
pub struct LuaCommandSnapshot {
    pub commands: Vec<LuaCommandInfo>,
    pub generation: u64,
}

#[derive(Clone)]
pub struct LuaCommandReader(Arc<ArcSwap<LuaCommandSnapshot>>);

impl LuaCommandReader {
    pub fn empty() -> Self {
        Self(Arc::new(ArcSwap::from_pointee(
            LuaCommandSnapshot::default(),
        )))
    }

    pub fn from_commands(commands: Vec<LuaCommandInfo>) -> Self {
        Self(Arc::new(ArcSwap::from_pointee(LuaCommandSnapshot {
            commands,
            generation: 1,
        })))
    }

    pub fn load(&self) -> arc_swap::Guard<Arc<LuaCommandSnapshot>> {
        self.0.load()
    }
}

pub(crate) struct LuaCommandWriter {
    store: Arc<ArcSwap<LuaCommandSnapshot>>,
    generation: AtomicU64,
}

impl LuaCommandWriter {
    pub fn new() -> (Self, LuaCommandReader) {
        let inner = Arc::new(ArcSwap::from_pointee(LuaCommandSnapshot::default()));
        (
            Self {
                store: Arc::clone(&inner),
                generation: AtomicU64::new(0),
            },
            LuaCommandReader(inner),
        )
    }

    pub fn publish(&self, commands: Vec<LuaCommandInfo>) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.store.store(Arc::new(LuaCommandSnapshot {
            commands,
            generation,
        }));
    }
}

pub(crate) struct CommandEntry {
    pub handler: RegistryKey,
    pub description: Arc<str>,
}

pub(crate) type CommandHandlerMap = HashMap<Arc<str>, HashMap<Arc<str>, CommandEntry>>;

pub(crate) fn publish_command_snapshot(map: &CommandHandlerMap, writer: &LuaCommandWriter) {
    let commands = map
        .iter()
        .flat_map(|(plugin, cmds)| {
            cmds.iter().map(move |(name, entry)| LuaCommandInfo {
                name: Arc::clone(name),
                description: Arc::clone(&entry.description),
                plugin: Arc::clone(plugin),
            })
        })
        .collect();
    writer.publish(commands);
}

pub struct SelectItem {
    pub label: String,
    pub detail: Option<String>,
}

pub struct SelectOpts {
    pub title: String,
    pub has_on_delete: bool,
    pub footer: Vec<(String, String)>,
}

pub enum SelectEvent {
    Choice { index: usize },
    Delete { index: usize },
    OpenEditor { index: usize },
    Close,
}

pub enum UiAction {
    Select {
        items: Vec<SelectItem>,
        opts: SelectOpts,
        reply_tx: flume::Sender<SelectEvent>,
    },
    Flash(String),
    OpenEditor(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    fn make_entry(lua: &Lua, desc: &str) -> CommandEntry {
        let f = lua.create_function(|_, ()| Ok(())).unwrap();
        let key = lua.create_registry_value(f).unwrap();
        CommandEntry {
            handler: key,
            description: Arc::from(desc),
        }
    }

    #[test]
    fn publish_snapshot_from_multiple_plugins() {
        let lua = Lua::new();
        let mut map: CommandHandlerMap = HashMap::new();
        map.entry(Arc::from("plugA"))
            .or_default()
            .insert(Arc::from("/cmd1"), make_entry(&lua, "desc1"));
        map.entry(Arc::from("plugA"))
            .or_default()
            .insert(Arc::from("/cmd2"), make_entry(&lua, "desc2"));
        map.entry(Arc::from("plugB"))
            .or_default()
            .insert(Arc::from("/cmd3"), make_entry(&lua, "desc3"));

        let (writer, reader) = LuaCommandWriter::new();
        publish_command_snapshot(&map, &writer);

        let snap = reader.load();
        assert_eq!(snap.commands.len(), 3);
        assert_eq!(snap.generation, 1);

        let names: Vec<&str> = snap.commands.iter().map(|c| c.name.as_ref()).collect();
        assert!(names.contains(&"/cmd1"));
        assert!(names.contains(&"/cmd2"));
        assert!(names.contains(&"/cmd3"));

        let plug_a_cmds: Vec<_> = snap
            .commands
            .iter()
            .filter(|c| c.plugin.as_ref() == "plugA")
            .collect();
        assert_eq!(plug_a_cmds.len(), 2);
    }

    #[test]
    fn publish_empty_map_produces_empty_snapshot() {
        let map: CommandHandlerMap = HashMap::new();
        let (writer, reader) = LuaCommandWriter::new();
        publish_command_snapshot(&map, &writer);

        let snap = reader.load();
        assert_eq!(snap.commands.len(), 0);
        assert_eq!(snap.generation, 1);
    }

    #[test]
    fn writer_generation_is_sequential() {
        let (writer, reader) = LuaCommandWriter::new();
        for i in 1..=5u64 {
            writer.publish(vec![]);
            assert_eq!(reader.load().generation, i);
        }
    }

    #[test]
    fn from_commands_constructor_sets_generation() {
        let reader = LuaCommandReader::from_commands(vec![LuaCommandInfo {
            name: Arc::from("/test"),
            description: Arc::from("d"),
            plugin: Arc::from("p"),
        }]);
        let snap = reader.load();
        assert_eq!(snap.commands.len(), 1);
        assert_eq!(snap.generation, 1);
    }
}
