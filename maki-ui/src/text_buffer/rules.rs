//! A line's rule marks are worked out the first time anyone asks about the
//! line, and kept until an edit touches it. So a keystroke matches only the
//! lines it changed.
//!
//! The frame, the cursor and the layout all ask the same way, and the answer
//! comes from the line's text alone. That is why a line far off screen, which
//! the layout counts but never draws, gets the same marks it would get on
//! screen.

use std::cell::OnceCell;
use std::iter::repeat_with;
use std::ops::Range;
use std::sync::{Arc, LazyLock};

use maki_agent::SpanStyle;
use maki_lua::{Decoration, InputRule, PatternRule, SyntaxRule};

use super::marks::Placed;
use crate::app::shell::parse_shell_prefix;

const SHELL_LANG: &str = "bash";
const SHELL_PREFIX_STYLE: &str = "shell_prefix";
const SHELL_COMMAND_RANK: usize = 0;
const SHELL_PREFIX_RANK: usize = 1;
const FIRST_PLUGIN_RANK: usize = 2;

static SHELL_COMMAND: LazyLock<Arc<Decoration>> = LazyLock::new(|| {
    Arc::new(Decoration {
        lang: Some(SHELL_LANG.into()),
        ..Decoration::default()
    })
});
static SHELL_PREFIX: LazyLock<Arc<Decoration>> = LazyLock::new(|| {
    Arc::new(Decoration {
        hl: Some(SpanStyle::Named(SHELL_PREFIX_STYLE.into())),
        ..Decoration::default()
    })
});

pub(super) type SyntaxRules = Arc<[(usize, Arc<SyntaxRule>)]>;

/// The host's own marks come first, so a plugin's rule draws over the `!` of
/// a shell line. The default has no rules at all, for inputs like a picker's
/// search where a `!` means nothing.
///
/// Every rule is ranked by its place in the list, and a tree-sitter rule
/// takes one rank per capture of each pattern, so its later patterns draw
/// over its earlier ones and so do the later captures of one pattern.
#[derive(Default)]
pub struct RuleSet {
    shell: bool,
    patterns: Vec<(usize, PatternRule)>,
    syntax: SyntaxRules,
    shapes_rows: bool,
}

impl RuleSet {
    pub fn with_plugins(plugin_rules: &[InputRule]) -> Arc<Self> {
        let mut patterns = Vec::new();
        let mut syntax = Vec::new();
        let mut rank = FIRST_PLUGIN_RANK;
        for rule in plugin_rules.iter().cloned() {
            match rule {
                InputRule::Pattern(rule) => {
                    patterns.push((rank, rule));
                    rank += 1;
                }
                InputRule::Syntax(rule) => {
                    let orders = rule.orders();
                    syntax.push((rank, rule));
                    rank += orders;
                }
            }
        }
        Arc::new(Self {
            shell: true,
            shapes_rows: patterns.iter().any(|(_, rule)| rule.mark().shapes_rows())
                || syntax.iter().any(|(_, rule)| rule.shapes_rows()),
            patterns,
            syntax: syntax.into(),
        })
    }

    pub(super) fn syntax(&self) -> &SyntaxRules {
        &self.syntax
    }
}

/// Offsets are kept from the start of the line, so an edit on a line above
/// moves the line without making its marks stale.
struct Derived {
    bytes: Range<usize>,
    decoration: Arc<Decoration>,
    rank: usize,
}

pub(super) struct RuleMarks {
    set: Arc<RuleSet>,
    lines: Vec<OnceCell<Box<[Derived]>>>,
}

impl RuleMarks {
    pub fn new(set: Arc<RuleSet>, lines: usize) -> Self {
        Self {
            set,
            lines: repeat_with(OnceCell::new).take(lines).collect(),
        }
    }

    pub fn shapes_rows(&self) -> bool {
        self.set.shapes_rows
    }

    pub fn splice(&mut self, old: Range<usize>, new_lines: usize) {
        self.lines
            .splice(old, repeat_with(OnceCell::new).take(new_lines));
    }

    pub fn line<'a>(
        &'a self,
        y: usize,
        text: &str,
        line: Range<usize>,
    ) -> impl Iterator<Item = Placed<'a>> {
        let derived = self.lines[y].get_or_init(|| {
            let start = line.start;
            let shell = (self.set.shell && start == 0)
                .then(|| shell_marks(&text[line.clone()]))
                .into_iter()
                .flatten();
            let patterns = self.set.patterns.iter().flat_map(|&(rank, ref rule)| {
                rule.matches(text, line.clone())
                    .map(move |(bytes, decoration)| Derived {
                        bytes: bytes.start - start..bytes.end - start,
                        decoration,
                        rank,
                    })
            });
            shell.chain(patterns).collect()
        });
        derived.iter().map(move |mark| {
            let bytes = line.start + mark.bytes.start..line.start + mark.bytes.end;
            Placed::derived(bytes, &mark.decoration, mark.rank)
        })
    }
}

/// The `!` of a prompt Enter runs as a shell line, and bash over the command
/// after it, read with `parse_shell_prefix` so the look cannot disagree with
/// the send. Only the first line counts, since a line's marks come from its
/// own text.
fn shell_marks(first_line: &str) -> impl Iterator<Item = Derived> {
    let lead = first_line.len() - first_line.trim_start().len();
    let prefix = parse_shell_prefix(first_line.trim());
    prefix.into_iter().flat_map(move |prefix| {
        let command = lead + prefix.prefix_len;
        [
            Derived {
                bytes: lead..command,
                decoration: Arc::clone(&SHELL_PREFIX),
                rank: SHELL_PREFIX_RANK,
            },
            Derived {
                bytes: command..first_line.len(),
                decoration: Arc::clone(&SHELL_COMMAND),
                rank: SHELL_COMMAND_RANK,
            },
        ]
    })
}
