//! A bench links this crate from outside, so the few private items it needs
//! to build an `App` and drive its input get a public door here.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::permissions::PermissionManager;
use maki_agent::{McpConfigErrors, McpSnapshotReader, SnapshotSpan, SpanStyle};
use maki_config::{ModelPolicy, PermissionsConfig, ProjectConfig, UiConfig};
use maki_lua::{
    EventHandle, HintReader, InputEdit, InputRule, InputRulesReader, KeymapReader,
    LuaCommandReader, PatternRule, PluginPermissions, PluginReach, Reveal, SyntaxRule, TextEdit,
};
use maki_providers::{Model, ModelTier};
use maki_storage::StateDir;
use ratatui::layout::Rect;

use crate::OpenSession;
use crate::app::App;
use crate::decor_worker;
use crate::storage_writer::StorageWriter;
use crate::text_buffer::{Decoration, Mark, RuleSet, VirtTextPos};

const CWD: &str = "/tmp";
const INPUT_HISTORY_SIZE: usize = 100;
const PLUGIN: &str = "bench";
const PROVIDER: &str = "anthropic";
const MARK_LEN: usize = 6;
const KIND_EVERY: usize = 10;
const GHOST: &str = "ghost";
const STYLE: &str = "bold";
const LANG: &str = "rust";
const SETTLE_TIMEOUT: Duration = Duration::from_secs(120);
const MARKDOWN_LANG: &str = "markdown";
const MARKDOWN_INLINE_LANG: &str = "markdown_inline";
const HEADING: &str = r"(?m)^#{1,6} .*";
const BOLD: &str = r"\*\*([^*\n]+)\*\*";
const CODE_SPAN: &str = r"`[^`\n]+`";
const LINK: &str = r"\[[^\]\n]+\]\([^)\n]+\)";
const FENCE_BLOCK_QUERY: &str = r#"
    (fenced_code_block
      (info_string (language) @injection.language)
      (code_fence_content) @injection.content)
    (fenced_code_block_delimiter) @hidden
"#;
const STRONG_QUERY: &str = "(strong_emphasis (emphasis_delimiter) @hidden) @strong";
const HIDDEN_CAPTURE: &str = "hidden";
const STRONG_CAPTURE: &str = "strong";

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
        InputRulesReader::empty(),
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
        anchor: None,
        version: app.input_box.buffer.version(),
        strict: false,
        session_id: app.state.session.id.to_string(),
        caller: PluginReach::new(PluginPermissions::trusted()).owned_by(Arc::from(PLUGIN)),
        marks: None,
    };
    app.apply_input_edit(edit, area).map(drop)
}

/// Spreads {count} marks evenly over the input, as one plugin's namespace.
/// Most are highlights, and one in ten each is inline virtual text, end of
/// line virtual text, or a `lang` range, so every kind is drawn and the
/// inline ones take part in wrapping.
pub fn decorate(app: &mut App, count: usize) {
    let text = app.input_box.buffer.value();
    let virt = |pos| {
        Arc::new(Decoration {
            virt_text: vec![SnapshotSpan {
                text: GHOST.into(),
                style: SpanStyle::Named(STYLE.into()),
            }],
            virt_text_pos: pos,
            ..Decoration::default()
        })
    };
    let looks = [
        virt(VirtTextPos::Inline),
        virt(VirtTextPos::Eol),
        Arc::new(Decoration {
            lang: Some(LANG.into()),
            ..Decoration::default()
        }),
        Arc::new(styled()),
    ];
    let marks = (0..count)
        .map(|i| {
            let start = text.floor_char_boundary(i * text.len() / count);
            let look = &looks[(i % KIND_EVERY).min(looks.len() - 1)];
            let stop = text.floor_char_boundary((start + MARK_LEN).min(text.len()));
            Mark {
                start,
                stop: (look.virt_text.is_empty() && stop > start).then_some(stop),
                decoration: Arc::clone(look),
            }
        })
        .collect();
    let version = app.input_box.buffer.version();
    app.input_box
        .set_marks(version, PLUGIN, PLUGIN, marks)
        .expect("marks fit the input");
}

fn styled() -> Decoration {
    Decoration {
        hl: Some(SpanStyle::Named(STYLE.into())),
        ..Decoration::default()
    }
}

pub fn set_cursor(app: &mut App, line: usize, x: usize) {
    app.input_box.buffer.set_cursor(line, x);
}

/// The rule sets a markdown prompt can run under. Both conceal the stars of
/// a bold span until the cursor is on it and color bold text, so each one
/// shapes rows and each one has the layout ask it about every line.
#[derive(Clone, Copy)]
pub enum Rules {
    /// Regexes, matched per line on the UI thread the first time the line is
    /// looked at.
    Regex,
    /// Tree-sitter markdown, parsed incrementally on the worker thread.
    Syntax,
}

pub fn set_rules(app: &mut App, rules: Rules) {
    let hidden = Decoration {
        conceal: Some(String::new()),
        reveal: Reveal::Cursor,
        ..Decoration::default()
    };
    let plugin_rules = match rules {
        Rules::Regex => {
            let pattern = |source, mark| {
                InputRule::Pattern(PatternRule::new(source, mark).expect("bench rules are valid"))
            };
            vec![
                pattern(HEADING, styled()),
                pattern(
                    BOLD,
                    Decoration {
                        conceal: Some("$1".into()),
                        ..hidden
                    },
                ),
                pattern(CODE_SPAN, styled()),
                pattern(LINK, styled()),
            ]
        }
        Rules::Syntax => {
            let rule = SyntaxRule::new(
                MARKDOWN_LANG,
                vec![
                    (MARKDOWN_LANG.into(), FENCE_BLOCK_QUERY.into()),
                    (MARKDOWN_INLINE_LANG.into(), STRONG_QUERY.into()),
                ],
                vec![
                    (HIDDEN_CAPTURE.into(), hidden),
                    (STRONG_CAPTURE.into(), styled()),
                ],
            )
            .expect("bench rules are valid");
            vec![InputRule::Syntax(Arc::new(rule))]
        }
    };
    app.input_box
        .buffer
        .set_rules(RuleSet::with_plugins(&plugin_rules));
}

/// Takes in worker results the way the event loop's ticks do, until the
/// tree-sitter marks are for the text as it is now. That is the state a
/// long typing session sits in between keystrokes. Returns at once when no
/// tree-sitter rule is set.
pub fn settle_parse(app: &mut App) {
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        app.input_box.poll_decor();
        if app.input_box.buffer.syntax_is_current() {
            return;
        }
        decor_worker::wakes()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the parse never finished");
    }
}
