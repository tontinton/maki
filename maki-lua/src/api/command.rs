use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use maki_agent::SharedBuf;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Abs(u16),
    Percent(u16),
}

impl Dimension {
    pub fn resolve(self, total: u16) -> u16 {
        match self {
            Self::Abs(n) => n,
            Self::Percent(p) => (total as u32 * p as u32 / 100) as u16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    #[default]
    NW,
    NE,
    SW,
    SE,
}

impl Anchor {
    pub fn parse(s: &str) -> Self {
        match s {
            "NE" => Self::NE,
            "SW" => Self::SW,
            "SE" => Self::SE,
            _ => Self::NW,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Border {
    None,
    Single,
    Double,
    #[default]
    Rounded,
}

impl Border {
    pub fn parse(s: &str) -> Self {
        match s {
            "none" => Self::None,
            "single" => Self::Single,
            "double" => Self::Double,
            _ => Self::Rounded,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TitlePos {
    #[default]
    Left,
    Center,
    Right,
}

impl TitlePos {
    pub fn parse(s: &str) -> Self {
        match s {
            "center" => Self::Center,
            "right" => Self::Right,
            _ => Self::Left,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloatConfig {
    pub width: Dimension,
    pub height: Dimension,
    pub row: Option<i16>,
    pub col: Option<i16>,
    pub anchor: Anchor,
    pub border: Border,
    pub title: String,
    pub title_pos: TitlePos,
    pub footer: Vec<(String, String)>,
    pub zindex: u16,
    pub cursor_line: bool,
    pub reserved_bottom: usize,
}

impl Default for FloatConfig {
    fn default() -> Self {
        Self {
            width: Dimension::Percent(60),
            height: Dimension::Percent(70),
            row: None,
            col: None,
            anchor: Anchor::default(),
            border: Border::default(),
            title: String::new(),
            title_pos: TitlePos::default(),
            footer: Vec::new(),
            zindex: 50,
            cursor_line: false,
            reserved_bottom: 0,
        }
    }
}

macro_rules! apply_opt {
    ($self:ident, $patch:ident, $($field:ident),+ $(,)?) => {
        $(if let Some(v) = $patch.$field { $self.$field = v; })+
    };
}

impl FloatConfig {
    pub fn apply_patch(&mut self, patch: FloatConfigPatch) {
        apply_opt!(
            self,
            patch,
            width,
            height,
            row,
            col,
            anchor,
            border,
            title,
            title_pos,
            footer,
            zindex,
            cursor_line,
            reserved_bottom
        );
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FloatConfigPatch {
    pub width: Option<Dimension>,
    pub height: Option<Dimension>,
    pub row: Option<Option<i16>>,
    pub col: Option<Option<i16>>,
    pub anchor: Option<Anchor>,
    pub border: Option<Border>,
    pub title: Option<String>,
    pub title_pos: Option<TitlePos>,
    pub footer: Option<Vec<(String, String)>>,
    pub zindex: Option<u16>,
    pub cursor_line: Option<bool>,
    pub reserved_bottom: Option<usize>,
}

pub enum WinEvent {
    Key { key: String, cursor: usize },
    Resize { width: u16, height: u16 },
    Close,
}

pub enum WinCommand {
    SetConfig(FloatConfigPatch),
    SetCursor(usize),
    Close,
}

pub enum UiAction {
    OpenWin {
        buf: Arc<SharedBuf>,
        config: FloatConfig,
        focus: bool,
        event_tx: flume::Sender<WinEvent>,
        cmd_rx: flume::Receiver<WinCommand>,
    },
    Flash(String),
    OpenEditor {
        path: PathBuf,
        reply_tx: flume::Sender<i32>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;
    use test_case::test_case;

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
    fn writer_generation_increments() {
        let (writer, reader) = LuaCommandWriter::new();
        writer.publish(vec![]);
        assert_eq!(reader.load().generation, 1);
        writer.publish(vec![]);
        assert_eq!(reader.load().generation, 2);
    }

    #[test_case(Dimension::Abs(42), 200 => 42 ; "abs_ignores_total")]
    #[test_case(Dimension::Percent(50), 200 => 100 ; "percent_half")]
    #[test_case(Dimension::Percent(100), 80 => 80 ; "percent_full")]
    #[test_case(Dimension::Percent(0), 80 => 0 ; "percent_zero")]
    #[test_case(Dimension::Percent(33), 100 => 33 ; "percent_truncates")]
    #[test_case(Dimension::Percent(1), 3 => 0 ; "percent_rounds_down_small")]
    fn dimension_resolve(dim: Dimension, total: u16) -> u16 {
        dim.resolve(total)
    }

    #[test_case("NW" => Anchor::NW ; "nw")]
    #[test_case("NE" => Anchor::NE ; "ne")]
    #[test_case("SW" => Anchor::SW ; "sw")]
    #[test_case("SE" => Anchor::SE ; "se")]
    #[test_case("garbage" => Anchor::NW ; "unknown_defaults_nw")]
    fn anchor_parse(s: &str) -> Anchor {
        Anchor::parse(s)
    }

    #[test_case("none" => Border::None ; "none")]
    #[test_case("single" => Border::Single ; "single")]
    #[test_case("double" => Border::Double ; "double")]
    #[test_case("rounded" => Border::Rounded ; "rounded")]
    #[test_case("unknown" => Border::Rounded ; "unknown_defaults_rounded")]
    fn border_parse(s: &str) -> Border {
        Border::parse(s)
    }

    #[test_case("center" => TitlePos::Center ; "center")]
    #[test_case("right" => TitlePos::Right ; "right")]
    #[test_case("left" => TitlePos::Left ; "left")]
    #[test_case("" => TitlePos::Left ; "empty_defaults_left")]
    fn title_pos_parse(s: &str) -> TitlePos {
        TitlePos::parse(s)
    }

    #[test]
    fn apply_patch_selective_fields() {
        let mut cfg = FloatConfig::default();
        let patch = FloatConfigPatch {
            title: Some("hello".into()),
            zindex: Some(99),
            cursor_line: Some(true),
            ..FloatConfigPatch::default()
        };
        cfg.apply_patch(patch);
        assert_eq!(cfg.title, "hello");
        assert_eq!(cfg.zindex, 99);
        assert!(cfg.cursor_line);
        assert_eq!(cfg.width, Dimension::Percent(60), "untouched fields stay");
        assert_eq!(cfg.border, Border::Rounded, "untouched fields stay");
    }

    #[test]
    fn apply_patch_row_col_option_option_semantics() {
        let mut cfg = FloatConfig {
            row: Some(10),
            col: Some(20),
            ..FloatConfig::default()
        };
        let patch = FloatConfigPatch {
            row: Some(None),
            col: Some(Some(5)),
            ..FloatConfigPatch::default()
        };
        cfg.apply_patch(patch);
        assert_eq!(cfg.row, None, "Some(None) clears the value");
        assert_eq!(cfg.col, Some(5), "Some(Some(5)) overwrites it");
    }

    #[test]
    fn apply_patch_empty_is_noop() {
        let original = FloatConfig::default();
        let mut cfg = original.clone();
        cfg.apply_patch(FloatConfigPatch::default());
        assert_eq!(cfg, original);
    }
}
