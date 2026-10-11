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

const SHELL_LANG: &str = "bash";
const SHELL_PREFIX_STYLE: &str = "shell_prefix";
/// A prompt starting with `!` runs as a shell command once something follows
/// the `!`, see `parse_shell_prefix`. `\A` is the start of the input, so a
/// later line starting with `!` stays plain text. The group is atomic, or a
/// lone `!!` would pass as a `!` followed by the command `!`.
const SHELL_PREFIX: &str = r"\A(?>!!? ?)(?=.*\S)";
/// Follows [`SHELL_PREFIX`]. `\K` drops the prefix from the match, so bash
/// colors the command alone.
const SHELL_COMMAND_AFTER_PREFIX: &str = r"\K.*";

pub(super) type SyntaxRules = Arc<[(usize, Arc<SyntaxRule>)]>;

static CORE: LazyLock<Vec<PatternRule>> = LazyLock::new(|| {
    let rule = |pattern: &str, mark| PatternRule::new(pattern, mark).expect("core rules are valid");
    vec![
        rule(
            &format!("{SHELL_PREFIX}{SHELL_COMMAND_AFTER_PREFIX}"),
            Decoration {
                lang: Some(SHELL_LANG.into()),
                ..Decoration::default()
            },
        ),
        rule(
            SHELL_PREFIX,
            Decoration {
                hl: Some(SpanStyle::Named(SHELL_PREFIX_STYLE.into())),
                ..Decoration::default()
            },
        ),
    ]
});

/// Core's rules come first, so a plugin's rule draws over the `!` at the
/// same priority. The default has no rules at all, for inputs like a
/// picker's search where a `!` means nothing.
///
/// Every rule is ranked by its place in the list, and a tree-sitter rule
/// takes one rank per pattern, so its later patterns draw over its earlier
/// ones.
#[derive(Default)]
pub struct RuleSet {
    patterns: Vec<(usize, PatternRule)>,
    syntax: SyntaxRules,
    shapes_rows: bool,
}

impl RuleSet {
    pub fn with_plugins(plugin_rules: &[InputRule]) -> Arc<Self> {
        let mut patterns = Vec::new();
        let mut syntax = Vec::new();
        let mut rank = 0;
        let core = CORE.iter().cloned().map(InputRule::Pattern);
        for rule in core.chain(plugin_rules.iter().cloned()) {
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
            self.set
                .patterns
                .iter()
                .flat_map(|&(rank, ref rule)| {
                    rule.matches(text, line.clone())
                        .map(move |(bytes, decoration)| Derived {
                            bytes: bytes.start - start..bytes.end - start,
                            decoration,
                            rank,
                        })
                })
                .collect()
        });
        derived.iter().map(move |mark| {
            let bytes = line.start + mark.bytes.start..line.start + mark.bytes.end;
            Placed::derived(bytes, &mark.decoration, mark.rank)
        })
    }
}
