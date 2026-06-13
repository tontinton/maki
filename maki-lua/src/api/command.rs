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

pub type HintEntries = Vec<(Arc<str>, Vec<(String, String)>)>;

#[derive(Clone, Default)]
pub struct HintSnapshot {
    pub entries: HintEntries,
    pub generation: u64,
}

#[derive(Clone)]
pub struct HintReader(Arc<ArcSwap<HintSnapshot>>);

impl HintReader {
    pub fn empty() -> Self {
        Self(Arc::new(ArcSwap::from_pointee(HintSnapshot::default())))
    }

    pub fn load(&self) -> arc_swap::Guard<Arc<HintSnapshot>> {
        self.0.load()
    }
}

pub(crate) struct HintWriter {
    store: Arc<ArcSwap<HintSnapshot>>,
    generation: AtomicU64,
}

impl HintWriter {
    pub fn new() -> (Self, HintReader) {
        let inner = Arc::new(ArcSwap::from_pointee(HintSnapshot::default()));
        (
            Self {
                store: Arc::clone(&inner),
                generation: AtomicU64::new(0),
            },
            HintReader(inner),
        )
    }

    pub fn publish(&self, entries: HintEntries) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.store.store(Arc::new(HintSnapshot {
            entries,
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
pub enum Split {
    #[default]
    None,
    Above,
    Below,
    Left,
    Right,
    Panel,
}

/// The renderer and layout read `Axis` rather than matching on `Split`, so a
/// new direction never needs a new match arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Reserves rows: a band on the top or bottom edge.
    Vertical,
    /// Reserves columns: a band on the left or right edge.
    Horizontal,
}

/// `at_start` means the top or left edge, otherwise the bottom or right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    pub axis: Axis,
    pub at_start: bool,
}

impl Split {
    pub fn parse(s: &str) -> Self {
        match s {
            "above" => Self::Above,
            "below" => Self::Below,
            "left" => Self::Left,
            "right" => Self::Right,
            "panel" => Self::Panel,
            _ => Self::None,
        }
    }

    /// The one place a direction turns into geometry, so the mapping lives in a
    /// single spot. `None` means the split is off.
    pub fn edge(self) -> Option<Edge> {
        Some(match self {
            Self::None | Self::Panel => return None,
            Self::Above => Edge {
                axis: Axis::Vertical,
                at_start: true,
            },
            Self::Below => Edge {
                axis: Axis::Vertical,
                at_start: false,
            },
            Self::Left => Edge {
                axis: Axis::Horizontal,
                at_start: true,
            },
            Self::Right => Edge {
                axis: Axis::Horizontal,
                at_start: false,
            },
        })
    }

    /// Every real direction, in carve order: columns before rows. Callers loop
    /// over this so they stay exhaustive without a match. It is hand-written, so
    /// `all_lists_exactly_the_edge_bearing_variants` keeps it honest.
    pub const ALL: [Split; 4] = [Split::Left, Split::Right, Split::Above, Split::Below];
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
    pub reserved_top: usize,
    pub split: Split,
    pub order: u16,
    pub visible: bool,
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
            reserved_top: 0,
            split: Split::None,
            order: 50,
            visible: true,
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
            reserved_bottom,
            reserved_top,
            split,
            order,
            visible
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
    pub reserved_top: Option<usize>,
    pub split: Option<Split>,
    pub order: Option<u16>,
    pub visible: Option<bool>,
}

pub enum WinEvent {
    Key { key: String },
    Resize { width: u16, height: u16 },
    Paste { text: String },
    Close,
}

pub enum WinCommand {
    SetConfig(FloatConfigPatch),
    SetCursor(usize),
    SetVisible(bool),
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

    #[test_case("panel" => Split::Panel ; "panel")]
    #[test_case("above" => Split::Above ; "above")]
    #[test_case("below" => Split::Below ; "below")]
    #[test_case("left" => Split::Left ; "left")]
    #[test_case("right" => Split::Right ; "right")]
    #[test_case("none" => Split::None ; "none")]
    #[test_case("" => Split::None ; "empty_defaults_none")]
    #[test_case("Below" => Split::None ; "exact_match_is_case_sensitive")]
    #[test_case("garbage" => Split::None ; "unknown_defaults_none")]
    fn split_parse(s: &str) -> Split {
        Split::parse(s)
    }

    #[test_case(Split::Panel => None ; "panel_has_no_edge")]
    #[test_case(Split::None => None ; "none_has_no_edge")]
    #[test_case(Split::Above => Some(Edge { axis: Axis::Vertical, at_start: true }) ; "above_is_vertical_start")]
    #[test_case(Split::Below => Some(Edge { axis: Axis::Vertical, at_start: false }) ; "below_is_vertical_end")]
    #[test_case(Split::Left => Some(Edge { axis: Axis::Horizontal, at_start: true }) ; "left_is_horizontal_start")]
    #[test_case(Split::Right => Some(Edge { axis: Axis::Horizontal, at_start: false }) ; "right_is_horizontal_end")]
    fn split_edge(split: Split) -> Option<Edge> {
        split.edge()
    }

    /// A variant belongs in `ALL` exactly when it carries an edge, so a dropped
    /// direction can never ship. The case list is exhaustive: add a variant and
    /// this fails until `ALL` agrees.
    #[test_case(Split::None)]
    #[test_case(Split::Above)]
    #[test_case(Split::Below)]
    #[test_case(Split::Left)]
    #[test_case(Split::Right)]
    #[test_case(Split::Panel)]
    fn all_lists_exactly_the_edge_bearing_variants(variant: Split) {
        assert_eq!(
            variant.edge().is_some(),
            Split::ALL.contains(&variant),
            "{variant:?}: edge-bearing variants belong in ALL, others must not",
        );
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
    fn apply_patch_sets_split() {
        let mut cfg = FloatConfig::default();
        assert_eq!(cfg.split, Split::None);
        let patch = FloatConfigPatch {
            split: Some(Split::Below),
            ..FloatConfigPatch::default()
        };
        cfg.apply_patch(patch);
        assert_eq!(cfg.split, Split::Below);
        assert_eq!(cfg.border, Border::Rounded, "untouched fields stay");
    }

    #[test]
    fn apply_patch_empty_is_noop() {
        let original = FloatConfig::default();
        let mut cfg = original.clone();
        cfg.apply_patch(FloatConfigPatch::default());
        assert_eq!(cfg, original);
    }

    #[test]
    fn hint_snapshot_publish_and_read() {
        let (writer, reader) = HintWriter::new();
        assert!(reader.load().entries.is_empty());

        writer.publish(vec![(
            Arc::from("plugA"),
            vec![(" 2/4 ".into(), "fg".into())],
        )]);

        let snap = reader.load();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.generation, 1);
    }
}
