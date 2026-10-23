//! Random edits, marks, undo and cursor moves against a [`TextBuffer`], each
//! checked against a plain model: [`ModelMark::remap`] for marks, the byte
//! names in [`Driver::atoms`] for stale edits, [`Driver::undo_texts`] for
//! undo. A failure replays with
//! `MAKI_FUZZ_SEED=N cargo nextest run -p maki-ui random_ops`.
//! `MAKI_FUZZ_SEEDS=N` runs the first N seeds instead of the default.

use std::collections::VecDeque;
use std::env;
use std::mem;
use std::ops::Range;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use fastrand::Rng;
use maki_agent::{SnapshotSpan, SpanStyle};
use maki_lua::{InputMarks, TextEdit};

use super::syntax::edit_since;
use super::test_support::text_edit;
use super::{
    CHANGE_LOG_LEN, Change, ChangeKind, Decoration, MAX_UNDO, Mark, Prompt, Reveal, TextBuffer,
    VirtTextPos, sanitize,
};

const SEED_VAR: &str = "MAKI_FUZZ_SEED";
const SEEDS_VAR: &str = "MAKI_FUZZ_SEEDS";
const SEEDS: u64 = 12;
pub(crate) const OPS_PER_SEED: usize = 300;

const PLUGINS: [&str; 2] = ["p1", "p2"];
const NAMESPACES: [&str; 2] = ["a", "b"];
const TYPED: [char; 9] = ['a', 'b', ' ', 'é', '一', '😀', 'z', '@', '1'];
/// Pasted and plugin text. Tabs, carriage returns and control characters
/// are cleaned on the way in, which shifts every offset after them.
const PIECES: [&str; 16] = [
    "ab", " ", "\n", "\t", "\r\n", "\r", "\u{7}", "é一", "😀", "xy\nz", "", "@ab", "#1", "**",
    "\n```\n", "`",
];
/// Whole values land verbatim, tabs included.
const VALUES: [&str; 5] = [
    "",
    "hello world",
    "one\ntwo\n\nthree 一二三",
    "a\tb\n😀 é",
    "abcdefghij\nklmnopqrst\nuvwxyz",
];
const CONCEALS: [&str; 4] = ["", "[+]", "@", "一"];
const VIRT_TEXTS: [&str; 2] = ["gh", "一x"];
/// A mark conceals, is atomic or carries virtual text one in this many
/// times, each on its own.
const FEATURE_ODDS: usize = 3;
const MAX_PIECES: usize = 3;
const MAX_EDITS: usize = 3;
const MAX_MARKS: usize = 4;
const MAX_MARK_LEN: usize = 24;
const MAX_LEN: usize = 240;
const MAX_CURSOR_X: usize = 14;
const LOG_TAIL: usize = 24;
/// Right below, at and right past what the log holds.
const BURSTS: [u64; 3] = [CHANGE_LOG_LEN - 1, CHANGE_LOG_LEN, CHANGE_LOG_LEN + 1];
/// One in this many generated edit lists or marks is malformed on purpose.
const MALFORMED_ODDS: usize = 10;
const STALE_ODDS: usize = 3;
const POINT_ODDS: usize = 4;
const CURSOR_ODDS: usize = 2;
const ANCHOR_ODDS: usize = 3;
const PUSHED_MARKS_ODDS: usize = 3;
/// Half the edits come with text that needs no cleaning, so the ones that
/// count offsets are not mostly refused for it.
const CLEAN_TEXT_ODDS: usize = 2;
const MAX_STALE: usize = 6;
const HISTORY_LEN: usize = MAX_STALE + 1;

const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;
const ALT: KeyModifiers = KeyModifiers::ALT;
const SUPER: KeyModifiers = KeyModifiers::SUPER;
const NONE: KeyModifiers = KeyModifiers::NONE;

const EDIT_KEYS: [(KeyCode, KeyModifiers); 8] = [
    (KeyCode::Backspace, NONE),
    (KeyCode::Delete, NONE),
    (KeyCode::Char('w'), CTRL),
    (KeyCode::Char('k'), CTRL),
    (KeyCode::Char('d'), ALT),
    (KeyCode::Backspace, ALT),
    (KeyCode::Backspace, SUPER),
    (KeyCode::Delete, CTRL),
];
const MOTION_KEYS: [(KeyCode, KeyModifiers); 11] = [
    (KeyCode::Left, NONE),
    (KeyCode::Right, NONE),
    (KeyCode::Up, NONE),
    (KeyCode::Down, NONE),
    (KeyCode::Home, NONE),
    (KeyCode::End, NONE),
    (KeyCode::Left, CTRL),
    (KeyCode::Right, ALT),
    (KeyCode::Char('a'), CTRL),
    (KeyCode::Char('e'), CTRL),
    (KeyCode::Right, SUPER),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Type,
    Newline,
    EditKey,
    UndoKey,
    Motion,
    Select,
    Paste,
    Edits,
    SetMarks,
    ClearMarks,
    Undo,
    Redo,
    RoundTrip,
    UndoAll,
    SetValue,
    SwapPrompt,
    SetCursor,
    ToggleRaw,
}

const OPS: [(Op, usize); 18] = [
    (Op::Type, 14),
    (Op::Newline, 3),
    (Op::EditKey, 6),
    (Op::UndoKey, 1),
    (Op::Motion, 8),
    (Op::Select, 4),
    (Op::Paste, 4),
    (Op::Edits, 6),
    (Op::SetMarks, 6),
    (Op::ClearMarks, 1),
    (Op::Undo, 3),
    (Op::Redo, 2),
    (Op::RoundTrip, 2),
    (Op::UndoAll, 1),
    (Op::SetValue, 1),
    (Op::SwapPrompt, 1),
    (Op::SetCursor, 3),
    (Op::ToggleRaw, 1),
];

/// The ops that change only the text, for runs that must not touch marks.
const TEXT_OPS: [Op; 6] = [
    Op::Type,
    Op::Newline,
    Op::EditKey,
    Op::Paste,
    Op::Undo,
    Op::Redo,
];

pub(crate) trait Odds {
    fn one_in(&mut self, n: usize) -> bool;
    fn pick<T: Copy>(&mut self, items: &[T]) -> T;
}

impl Odds for Rng {
    fn one_in(&mut self, n: usize) -> bool {
        self.usize(..n) == 0
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.usize(..items.len())]
    }
}

/// The one seed in `MAKI_FUZZ_SEED`, else the first `MAKI_FUZZ_SEEDS` or
/// {default}.
pub(crate) fn seeds(default: u64) -> Range<u64> {
    match env::var(SEED_VAR) {
        Ok(seed) => {
            let seed: u64 = seed
                .parse()
                .unwrap_or_else(|_| panic!("{SEED_VAR} must be a number"));
            seed..seed + 1
        }
        Err(_) => {
            0..env::var(SEEDS_VAR).map_or(default, |count| {
                count
                    .parse()
                    .unwrap_or_else(|_| panic!("{SEEDS_VAR} must be a number"))
            })
        }
    }
}

/// All a refused edit leaves alone.
#[derive(PartialEq)]
struct Snapshot {
    text: String,
    version: u64,
    cursor: usize,
    anchor: Option<usize>,
    marks: Vec<(usize, Range<usize>)>,
    any_atomic: bool,
}

/// A placed mark as the model keeps it: no chunks, no shifts, one plain list.
struct ModelMark {
    plugin: &'static str,
    ns: &'static str,
    mark: Mark,
}

impl ModelMark {
    /// Reads {change} as deleting `start..old_end` and then inserting
    /// `new_end - start` bytes at `start`. An offset at the insert stays in
    /// front of it unless it sticks to what comes after. A point always does.
    /// A range keeps its edges out of text typed at them, goes when it ends
    /// up empty, and goes when a conceal or atomic range has its own bytes
    /// edited.
    fn remap(&mut self, change: &Change) -> bool {
        let mark = &mut self.mark;
        let deleted = change.old_end - change.start;
        let inserted = change.new_end - change.start;
        let through = |pos: usize, sticks_after: bool| {
            let pos = if pos <= change.start {
                pos
            } else {
                pos.max(change.old_end) - deleted
            };
            if pos > change.start || pos == change.start && sticks_after {
                pos + inserted
            } else {
                pos
            }
        };
        let Some(stop) = mark.stop else {
            mark.start = through(mark.start, true);
            return true;
        };
        let decoration = &mark.decoration;
        let edits_own_bytes = if deleted == 0 {
            mark.start < change.start && change.start < stop
        } else {
            change.start < stop && change.old_end > mark.start
        };
        if edits_own_bytes && (decoration.conceal.is_some() || decoration.atomic) {
            return false;
        }
        let start = through(mark.start, true);
        let stop = through(stop, false);
        mark.start = start;
        mark.stop = Some(stop);
        start < stop
    }
}

fn floor_boundary(text: &str, at: usize) -> usize {
    text.floor_char_boundary(at.min(text.len()))
}

/// A byte inside a multi-byte char of {text}, if it has one.
fn inside_char(text: &str) -> Option<usize> {
    text.char_indices()
        .find(|(_, c)| c.len_utf8() > 1)
        .map(|(at, _)| at + 1)
}

pub(crate) struct Driver {
    pub buffer: TextBuffer,
    pub rng: Rng,
    seed: u64,
    model: Vec<ModelMark>,
    /// The version {model} is mapped to.
    seen: u64,
    /// A name for every byte of the value, kept through every change, so a
    /// byte can be found again after the text around it moved.
    atoms: Vec<u64>,
    next_atom: u64,
    /// The text and the bytes at each recent version, oldest first.
    history: VecDeque<(u64, String, Vec<u64>)>,
    /// The text before each action since the last whole-value write, oldest
    /// first. Undo lands on the one before the step it takes back, which may
    /// have joined a few actions into one. A key typed over a selection that
    /// puts back the same text leaves two equal texts the model cannot tell
    /// were joined, so undo may run out with only such texts left.
    undo_texts: Vec<String>,
    /// What each undo took off {undo_texts}, and the text it left.
    redo_texts: Vec<(String, Vec<String>)>,
    /// More actions than the undo history keeps, so its oldest are gone.
    truncated: bool,
    raw: bool,
    /// The prompt a swap put away, like a draft parked in another chat, and
    /// the model of its marks.
    stashed: (Prompt, Vec<ModelMark>),
    log: VecDeque<String>,
    /// Replaying one seed keeps every op, not just the last few.
    full_log: bool,
}

impl Driver {
    pub fn new(seed: u64) -> Self {
        let mut rng = Rng::with_seed(seed);
        let value = rng.pick(&VALUES);
        let buffer = TextBuffer::new(value.into());
        let atoms: Vec<u64> = (0..value.len() as u64).collect();
        Self {
            history: VecDeque::from([(buffer.version(), value.to_owned(), atoms.clone())]),
            next_atom: atoms.len() as u64,
            atoms,
            seen: buffer.version(),
            buffer,
            rng,
            seed,
            model: Vec::new(),
            undo_texts: Vec::new(),
            redo_texts: Vec::new(),
            truncated: false,
            raw: false,
            stashed: Default::default(),
            log: VecDeque::new(),
            full_log: env::var_os(SEED_VAR).is_some(),
        }
    }

    /// The seed and the last ops, for an assert message.
    pub fn context(&self) -> String {
        let ops: Vec<&str> = self.log.iter().map(String::as_str).collect();
        format!(
            "seed {} ({SEED_VAR}={} replays it), value {:?}, cursor {}, anchor {:?}, last ops:\n  {}",
            self.seed,
            self.seed,
            self.buffer.value(),
            self.buffer.cursor_byte(),
            self.buffer.anchor(),
            ops.join("\n  ")
        )
    }

    pub(crate) fn note(&mut self, op: String) {
        if !self.full_log && self.log.len() == LOG_TAIL {
            self.log.pop_front();
        }
        self.log.push_back(op);
    }

    /// One random op, checked. Returns where marks that change how rows wrap
    /// came or went, which the layout has to be told about.
    pub fn step(&mut self) -> Vec<Range<usize>> {
        let total: usize = OPS.iter().map(|(_, weight)| weight).sum();
        let mut roll = self.rng.usize(..total);
        let op = OPS
            .iter()
            .find(|(_, weight)| {
                let hit = roll < *weight;
                roll = roll.saturating_sub(*weight);
                hit
            })
            .map_or(Op::Type, |(op, _)| *op);
        self.run(op)
    }

    /// Ops that only change the text, until the value has moved about as
    /// many versions on as the log holds.
    pub fn text_burst(&mut self) {
        let target = self.buffer.version() + self.rng.pick(&BURSTS);
        while self.buffer.version() < target {
            let op = self.rng.pick(&TEXT_OPS);
            self.run(op);
        }
    }

    fn run(&mut self, op: Op) -> Vec<Range<usize>> {
        if self.buffer.value().len() > MAX_LEN {
            self.trim();
            return Vec::new();
        }
        let version = self.buffer.version();
        let text = self.buffer.value().to_owned();
        let moved = match op {
            Op::Type => {
                let c = self.rng.pick(&TYPED);
                self.note(format!("type {c:?}"));
                self.buffer
                    .handle_key(KeyEvent::new(KeyCode::Char(c), NONE));
                Vec::new()
            }
            Op::Newline => {
                self.note("newline".into());
                self.buffer.add_line();
                Vec::new()
            }
            Op::EditKey => {
                let (code, modifiers) = self.rng.pick(&EDIT_KEYS);
                self.note(format!("key {modifiers:?}+{code:?}"));
                self.buffer.handle_key(KeyEvent::new(code, modifiers));
                Vec::new()
            }
            Op::UndoKey => {
                self.note("key Super+z".into());
                self.buffer
                    .handle_key(KeyEvent::new(KeyCode::Char('z'), SUPER));
                self.undone(version, text);
                self.check(op);
                return Vec::new();
            }
            Op::Motion | Op::Select => {
                let (code, modifiers) = self.rng.pick(&MOTION_KEYS);
                let select = op == Op::Select;
                self.note(format!("motion {modifiers:?}+{code:?} select={select}"));
                if select {
                    self.buffer
                        .handle_motion_key(KeyEvent::new(code, modifiers | SHIFT), true);
                } else {
                    self.buffer.handle_key(KeyEvent::new(code, modifiers));
                }
                Vec::new()
            }
            Op::Paste => {
                let text = self.text();
                self.note(format!("paste {text:?}"));
                self.buffer.insert_text(&text);
                Vec::new()
            }
            Op::Edits => self.edits(),
            Op::SetMarks => self.set_marks(),
            Op::ClearMarks => {
                let plugin = self.rng.pick(&PLUGINS);
                self.note(format!("clear {plugin}"));
                self.model.retain(|mark| mark.plugin != plugin);
                self.buffer.clear_plugin_marks(plugin)
            }
            Op::Undo => {
                self.note("undo".into());
                self.undo();
                self.check(op);
                return Vec::new();
            }
            Op::Redo => {
                self.note("redo".into());
                self.redo();
                self.check(op);
                return Vec::new();
            }
            Op::RoundTrip => {
                self.round_trip();
                self.check(op);
                return Vec::new();
            }
            Op::UndoAll => {
                self.undo_all();
                self.check(op);
                return Vec::new();
            }
            Op::SetValue => {
                let value = self.rng.pick(&VALUES);
                self.note(format!("set_value {value:?}"));
                self.model.clear();
                self.buffer.set_value(value);
                self.swapped();
                self.check(op);
                return Vec::new();
            }
            Op::SwapPrompt => {
                self.note("swap_prompt".into());
                let (prompt, model) = mem::take(&mut self.stashed);
                let shown_model = mem::take(&mut self.model);
                let shown = self.buffer.swap_prompt(prompt);
                self.swapped();
                self.model = model;
                self.stashed = (shown, shown_model);
                self.check(op);
                return Vec::new();
            }
            Op::SetCursor => {
                let y = self.rng.usize(..self.buffer.line_count() + 1);
                let x = self.rng.usize(..MAX_CURSOR_X);
                self.note(format!("click {y} {x}"));
                self.buffer.set_cursor(y, x);
                self.buffer.deselect();
                Vec::new()
            }
            Op::ToggleRaw => {
                self.note("toggle_raw".into());
                self.buffer.toggle_raw();
                self.raw = !self.raw;
                Vec::new()
            }
        };
        self.acted(version, text);
        self.check(op);
        moved
    }

    /// A whole prompt came in, which starts the undo history over.
    fn swapped(&mut self) {
        self.record();
        self.undo_texts.clear();
        self.redo_texts.clear();
        self.truncated = false;
    }

    /// Deletes most of a value grown too long for the checks to stay cheap.
    fn trim(&mut self) {
        let (version, text) = (self.buffer.version(), self.buffer.value().to_owned());
        let stop = floor_boundary(&text, text.len() - MAX_LEN / 2);
        self.note(format!("trim 0..{stop}"));
        let edit = text_edit(0, stop, "");
        self.buffer
            .apply_edits(self.buffer.version(), vec![edit], None, None, None)
            .unwrap_or_else(|err| panic!("trim refused: {err}\n{}", self.context()));
        self.acted(version, text);
        self.check(Op::Edits);
    }

    /// An action that changed the value from {text} at {version} made an
    /// undo step, and nothing is left to redo. One that only wrote text over
    /// the same text changed nothing, so it leaves both alone. An action's
    /// edits never overlap, so that is every change keeping its length with
    /// the value the same.
    fn acted(&mut self, version: u64, text: String) {
        self.record();
        let rewrote_the_same = self.buffer.value() == text
            && self
                .buffer
                .changes_since(version)
                .expect("the log reaches one version back")
                .all(|change| change.old_end == change.new_end);
        if self.buffer.version() != version && !rewrote_the_same {
            self.undo_texts.push(text);
            self.redo_texts.clear();
            self.truncated |= self.undo_texts.len() > MAX_UNDO;
        }
    }

    fn undo(&mut self) -> bool {
        let (version, text) = (self.buffer.version(), self.buffer.value().to_owned());
        self.buffer.undo();
        self.undone(version, text)
    }

    /// Checks what an undo of the value {text} at {version} did.
    fn undone(&mut self, version: u64, text: String) -> bool {
        self.record();
        if self.buffer.version() == version {
            assert!(
                self.truncated || self.undo_texts.iter().all(|before| *before == text),
                "nothing to undo with {} actions made\n{}",
                self.undo_texts.len(),
                self.context()
            );
            return false;
        }
        let landed = self
            .undo_texts
            .iter()
            .rposition(|before| before == self.buffer.value())
            .unwrap_or_else(|| {
                panic!(
                    "undo landed on a text no action started from\n{}",
                    self.context()
                )
            });
        let taken = self.undo_texts.split_off(landed);
        self.redo_texts.push((text, taken));
        true
    }

    fn redo(&mut self) -> bool {
        let version = self.buffer.version();
        self.buffer.redo();
        self.record();
        if self.buffer.version() == version {
            assert!(
                self.redo_texts.is_empty(),
                "nothing to redo after an undo\n{}",
                self.context()
            );
            return false;
        }
        let (text, taken) = self
            .redo_texts
            .pop()
            .unwrap_or_else(|| panic!("redo with nothing undone\n{}", self.context()));
        assert_eq!(self.buffer.value(), text, "{}", self.context());
        self.undo_texts.extend(taken);
        true
    }

    fn text(&mut self) -> String {
        let pieces = 1 + self.rng.usize(..MAX_PIECES);
        (0..pieces).map(|_| self.rng.pick(&PIECES)).collect()
    }

    /// A version the value had lately, with its text and bytes.
    fn some_version(&mut self) -> (u64, String, Vec<u64>) {
        let back = if self.rng.one_in(STALE_ODDS) {
            self.rng.usize(..MAX_STALE.min(self.history.len()))
        } else {
            0
        };
        self.history[self.history.len() - 1 - back].clone()
    }

    fn boundary(&mut self, text: &str) -> usize {
        floor_boundary(text, self.rng.usize(..text.len() + 1))
    }

    /// Each look on its own or mixed with the others, since a chip is often
    /// atomic, concealed and carries virtual text all at once.
    fn decoration(&mut self) -> Arc<Decoration> {
        let conceal = self
            .rng
            .one_in(FEATURE_ODDS)
            .then(|| self.rng.pick(&CONCEALS).to_owned());
        let virt_text = if self.rng.one_in(FEATURE_ODDS) {
            vec![SnapshotSpan {
                text: self.rng.pick(&VIRT_TEXTS).into(),
                style: SpanStyle::Default,
            }]
        } else {
            Vec::new()
        };
        Arc::new(Decoration {
            conceal,
            reveal: self.rng.pick(&[Reveal::Cursor, Reveal::Never]),
            atomic: self.rng.one_in(FEATURE_ODDS),
            virt_text,
            virt_text_pos: self.rng.pick(&[VirtTextPos::Inline, VirtTextPos::Eol]),
            ..Decoration::default()
        })
    }

    fn marks_for(&mut self, text: &str) -> Vec<Mark> {
        (0..self.rng.usize(..MAX_MARKS + 1))
            .map(|_| {
                let start = self.boundary(text);
                let stop = (!self.rng.one_in(POINT_ODDS)).then(|| {
                    floor_boundary(text, start + self.rng.usize(..MAX_MARK_LEN)).max(start)
                });
                Mark {
                    start,
                    stop,
                    decoration: self.decoration(),
                }
            })
            .collect()
    }

    fn model_marks(plugin: &'static str, ns: &'static str, marks: &[Mark]) -> Vec<ModelMark> {
        marks
            .iter()
            .filter(|mark| mark.stop != Some(mark.start))
            .map(|mark| ModelMark {
                plugin,
                ns,
                mark: mark.clone(),
            })
            .collect()
    }

    fn set_marks(&mut self) -> Vec<Range<usize>> {
        let plugin = self.rng.pick(&PLUGINS);
        let ns = self.rng.pick(&NAMESPACES);
        let (version, text, _) = self.some_version();
        let mut marks = self.marks_for(&text);
        let reversed = self.rng.one_in(MALFORMED_ODDS) && !marks.is_empty();
        if reversed {
            let mark = &mut marks[0];
            mark.stop = Some(mark.start);
            mark.start += 1;
        }
        self.note(format!(
            "set_marks {plugin}/{ns} at v{version} (now v{}): {:?}",
            self.buffer.version(),
            marks
                .iter()
                .map(|mark| (mark.start, mark.stop, mark.decoration.as_ref()))
                .collect::<Vec<_>>()
        ));
        let changes: Option<Vec<Change>> = self
            .buffer
            .changes_since(version)
            .map(|changes| changes.cloned().collect());
        let expect_ok = !reversed
            && changes
                .as_ref()
                .is_some_and(|changes| changes.iter().all(|c| c.kind == ChangeKind::Edit));
        let result = self.buffer.set_marks(version, plugin, ns, marks.clone());
        assert_eq!(
            result.is_ok(),
            expect_ok,
            "set_marks gave {result:?}, reversed {reversed}, log reaches v{version} {}\n{}",
            changes.is_some(),
            self.context()
        );
        let Ok(moved) = result else {
            return Vec::new();
        };
        self.model
            .retain(|mark| mark.plugin != plugin || mark.ns != ns);
        let mut placed = Self::model_marks(plugin, ns, &marks);
        placed.retain_mut(|mark| changes.iter().flatten().all(|change| mark.remap(change)));
        self.model.extend(placed);
        moved
    }

    /// A plugin edit planned on a recent version. Where it should land now
    /// is worked out from which bytes of that version survived, not from the
    /// log, and the cursor counted in the value it would have made back then
    /// lands where those bytes went. Some have no edits, and only move the
    /// cursor or the selection or replace marks.
    fn edits(&mut self) -> Vec<Range<usize>> {
        let (version, old, old_atoms) = self.some_version();
        let current = version == self.buffer.version();
        let mut cuts: Vec<usize> = (0..2 * self.rng.usize(..MAX_EDITS + 1))
            .map(|_| self.boundary(&old))
            .collect();
        cuts.sort_unstable();
        let mut edits: Vec<TextEdit> = cuts
            .chunks(2)
            .map(|pair| TextEdit {
                start: pair[0],
                stop: pair[1],
                text: self.text(),
            })
            .collect();
        let malformed = self.rng.one_in(MALFORMED_ODDS);
        if !malformed {
            edits.dedup_by_key(|edit| edit.start);
        }
        if self.rng.one_in(CLEAN_TEXT_ODDS) {
            for edit in &mut edits {
                edit.text = sanitize(&edit.text);
            }
        }
        let clean: Vec<(Range<usize>, String)> = edits
            .iter()
            .map(|edit| (edit.start..edit.stop, sanitize(&edit.text)))
            .collect();
        let unclean = edits
            .iter()
            .zip(&clean)
            .any(|(edit, (_, text))| edit.text != *text);

        let mut planned = old.clone();
        for (range, text) in clean.iter().rev() {
            planned.replace_range(range.clone(), text);
        }
        let (mut cursor, mut anchor, mut pushed) = (None, None, None);
        if self.rng.one_in(CURSOR_ODDS) {
            cursor = Some(match inside_char(&planned) {
                Some(inside) if current && self.rng.one_in(MALFORMED_ODDS) => inside,
                _ => self.boundary(&planned),
            });
        }
        if current && self.rng.one_in(ANCHOR_ODDS) {
            anchor = Some(self.boundary(&planned));
        }
        let plugin = self.rng.pick(&PLUGINS);
        let ns = self.rng.pick(&NAMESPACES);
        if current && self.rng.one_in(PUSHED_MARKS_ODDS) {
            pushed = Some(self.marks_for(&planned));
        }
        let off_boundary = cursor.is_some_and(|at| !planned.is_char_boundary(at));
        let counts_offsets = cursor.is_some() || anchor.is_some() || pushed.is_some();
        let replaced = self
            .buffer
            .changes_since(version)
            .is_none_or(|mut changes| changes.any(|change| change.kind == ChangeKind::Replace));
        let landing = (!malformed && !replaced)
            .then(|| self.rebased(&old_atoms, &clean))
            .flatten();
        let expected = landing.as_ref().map(|ranges| {
            let mut value = self.buffer.value().to_owned();
            for (range, (_, text)) in ranges.iter().zip(&clean).rev() {
                value.replace_range(range.clone(), text);
            }
            let caret = self.caret_after(&old_atoms, &clean, ranges, cursor);
            (value, caret)
        });

        self.note(format!(
            "apply_edits at v{version} (now v{}) {:?} cursor {cursor:?} anchor {anchor:?} marks {:?}",
            self.buffer.version(),
            edits
                .iter()
                .map(|edit| (edit.start, edit.stop, edit.text.as_str()))
                .collect::<Vec<_>>(),
            pushed.as_ref().map(|marks| marks
                .iter()
                .map(|mark| (mark.start, mark.stop, mark.decoration.as_ref()))
                .collect::<Vec<_>>())
        ));
        let before = self.snapshot();
        let input_marks = pushed.clone().map(|marks| {
            (
                plugin,
                InputMarks {
                    ns: ns.into(),
                    marks,
                },
            )
        });
        let result = self
            .buffer
            .apply_edits(version, edits, cursor, anchor, input_marks);
        let nothing_to_apply =
            clean.is_empty() && cursor.is_none() && anchor.is_none() && pushed.is_none();
        let refusable = off_boundary || unclean && counts_offsets || nothing_to_apply;
        // A stale edit the model can land may still be refused. The buffer
        // reads the log, so typing that only touches an edited range, or a
        // cursor in text changed since, refuses it, though the bytes the
        // model follows survived.
        if !malformed && (current || expected.is_none()) {
            assert_eq!(
                result.is_ok(),
                expected.is_some() && !refusable,
                "apply_edits gave {result:?}, lands {}, off_boundary {off_boundary}, \
                 unclean {unclean}, counts_offsets {counts_offsets}, \
                 nothing_to_apply {nothing_to_apply}\n{}",
                expected.is_some(),
                self.context()
            );
        }
        match result {
            Err(_) => {
                assert!(
                    self.snapshot() == before,
                    "a refused edit changed the buffer\n{}",
                    self.context()
                );
                Vec::new()
            }
            Ok((caret, moved)) => {
                assert_eq!(
                    caret,
                    self.buffer.cursor_byte(),
                    "the caret returned is not the cursor\n{}",
                    self.context()
                );
                if let Some((value, expected_caret)) = expected {
                    assert_eq!(
                        self.buffer.value(),
                        value,
                        "the edit landed unlike the model\n{}",
                        self.context()
                    );
                    if let Some(expected_caret) = expected_caret {
                        assert!(
                            caret == expected_caret || self.may_not_stop_at(expected_caret),
                            "the caret {caret} is not the planned {expected_caret}\n{}",
                            self.context()
                        );
                    }
                }
                if let Some(marks) = pushed {
                    // Map the old marks through this edit first, since the
                    // pushed ones already count in its result.
                    self.record();
                    self.model
                        .retain(|mark| mark.plugin != plugin || mark.ns != ns);
                    self.model.extend(Self::model_marks(plugin, ns, &marks));
                }
                moved
            }
        }
    }

    /// Where {edits}, planned on the value whose bytes were {old}, cut the
    /// value now: around the same bytes, which have to have survived side
    /// by side. An insert goes before the byte it was planned in front of.
    fn rebased(&self, old: &[u64], edits: &[(Range<usize>, String)]) -> Option<Vec<Range<usize>>> {
        edits
            .iter()
            .map(|(range, _)| {
                let start = self.now_at(old, range.start)?;
                if range.is_empty() {
                    return Some(start..start);
                }
                let stop = self.now_at(old, range.end - 1)? + 1;
                (stop - start == range.len()).then_some(start..stop)
            })
            .collect()
    }

    /// Where the byte at {at} of the value whose bytes were {old} is now, the
    /// end for the end. `None` when that byte is gone.
    fn now_at(&self, old: &[u64], at: usize) -> Option<usize> {
        match old.get(at) {
            Some(atom) => self.atoms.iter().position(|now| now == atom),
            None => Some(self.atoms.len()),
        }
    }

    /// Where the caret ends up after {edits} land on {landing} in the value
    /// now: the end of the last edit, where it is with no edits, or {cursor},
    /// counted in the value they would have made of {old}. Inside an edit's
    /// text it stays there, and elsewhere it goes before the byte of {old} it
    /// was in front of. `None` when that byte is gone, where the answer
    /// depends on how.
    fn caret_after(
        &self,
        old: &[u64],
        edits: &[(Range<usize>, String)],
        landing: &[Range<usize>],
        cursor: Option<usize>,
    ) -> Option<usize> {
        let mut grown = 0;
        let starts: Vec<usize> = landing
            .iter()
            .zip(edits)
            .map(|(range, (_, text))| {
                let start = range.start.wrapping_add_signed(grown);
                grown += text.len().cast_signed() - range.len().cast_signed();
                start
            })
            .collect();
        let Some(cursor) = cursor else {
            return match (starts.last(), edits.last()) {
                (Some(start), Some((_, text))) => Some(start + text.len()),
                _ => Some(self.buffer.cursor_byte()),
            };
        };
        let mut shift = 0;
        for ((range, text), start) in edits.iter().zip(&starts) {
            let planned = range.start.wrapping_add_signed(shift);
            if cursor < planned {
                break;
            }
            if cursor <= planned + text.len() {
                return Some(start + cursor - planned);
            }
            shift += text.len().cast_signed() - range.len().cast_signed();
        }
        let at = cursor.wrapping_add_signed(-shift);
        let now = self.now_at(old, at)?;
        let before: isize = landing
            .iter()
            .zip(edits)
            .filter(|(range, _)| range.start < now)
            .map(|(range, (_, text))| text.len().cast_signed() - range.len().cast_signed())
            .sum();
        Some(now.wrapping_add_signed(before))
    }

    fn snapshot(&self) -> Snapshot {
        let buf = &self.buffer;
        Snapshot {
            text: buf.value().to_owned(),
            version: buf.version(),
            cursor: buf.cursor_byte(),
            anchor: buf.anchor(),
            marks: self.placed(),
            any_atomic: buf.atomic(0..=buf.value().len()).next().is_some(),
        }
    }

    /// Undo then redo puts back the text, the cursor and the selection, and
    /// so does redo then undo. A selection is widened over the atomic ranges
    /// it lands in, which the undo and redo themselves may move, so with an
    /// atomic range around, placed or from a rule, only the text and the
    /// cursor have to come back.
    fn round_trip(&mut self) {
        self.note("undo+redo".into());
        let before = self.snapshot();
        if self.undo() {
            assert!(self.redo(), "nothing to redo\n{}", self.context());
            self.assert_same_selection(&before);
        }
        self.note("redo+undo".into());
        if self.redo() {
            assert!(self.undo(), "nothing to undo\n{}", self.context());
            self.assert_same_selection(&before);
        }
    }

    /// Undo walks back to the last whole-value write unless more actions
    /// were made than it keeps, and as many redos walk forward to where it
    /// started.
    fn undo_all(&mut self) {
        self.note(format!(
            "undo all ({} actions, truncated {})",
            self.undo_texts.len(),
            self.truncated
        ));
        let before = self.buffer.value().to_owned();
        let mut undone = 0;
        while self.undo() {
            undone += 1;
            assert!(undone <= MAX_UNDO, "undo never ran out\n{}", self.context());
        }
        for _ in 0..undone {
            assert!(self.redo(), "redo ran out\n{}", self.context());
        }
        assert_eq!(self.buffer.value(), before, "{}", self.context());
    }

    /// Maps the model through what changed since it last looked, and checks
    /// the log against the texts it remembers.
    fn record(&mut self) {
        let version = self.buffer.version();
        if version == self.seen {
            return;
        }
        assert_eq!(
            version,
            self.seen + 1,
            "more than one version since the last op\n{}",
            self.context()
        );
        let changes: Vec<Change> = self
            .buffer
            .changes_since(self.seen)
            .expect("the log reaches one version back")
            .cloned()
            .collect();
        let grown: isize = changes
            .iter()
            .map(|c| c.new_end.cast_signed() - c.old_end.cast_signed())
            .sum();
        let old_len = self.history.back().map_or(0, |(_, text, _)| text.len());
        assert_eq!(
            old_len.checked_add_signed(grown),
            Some(self.buffer.value().len()),
            "the logged changes do not add up to the new length\n{}",
            self.context()
        );
        self.model
            .retain_mut(|mark| changes.iter().all(|change| mark.remap(change)));
        for change in &changes {
            let inserted = change.new_end - change.start;
            let fresh = self.next_atom..self.next_atom + inserted as u64;
            self.next_atom = fresh.end;
            self.atoms.splice(change.start..change.old_end, fresh);
        }
        self.seen = version;
        if self.history.len() == HISTORY_LEN {
            self.history.pop_front();
        }
        self.history
            .push_back((version, self.buffer.value().to_owned(), self.atoms.clone()));
        for (old, text, _) in &self.history {
            let changes = self.buffer.changes_since(*old).expect("a recent version");
            let Some(edit) = edit_since(changes) else {
                continue;
            };
            let now = self.buffer.value();
            let spliced = [
                &text[..edit.start],
                &now[edit.start..edit.new_end],
                &text[edit.old_end..],
            ]
            .concat();
            assert_eq!(spliced, now, "one edit since v{old}\n{}", self.context());
        }
        let edge = version.saturating_sub(CHANGE_LOG_LEN);
        for old in [edge.saturating_sub(1), edge, edge + 1] {
            assert_eq!(
                self.buffer.changes_since(old).is_some(),
                version - old <= CHANGE_LOG_LEN,
                "the log reach is off for v{old} at v{version}\n{}",
                self.context()
            );
        }
    }

    fn placed(&self) -> Vec<(usize, Range<usize>)> {
        let mut placed: Vec<_> = self
            .buffer
            .marks()
            .touching(0..=usize::MAX)
            .map(|mark| (mark.decoration as *const Decoration as usize, mark.bytes))
            .collect();
        placed.sort_by_key(|(id, bytes)| (*id, bytes.start, bytes.end));
        placed
    }

    fn expected_placed(&self) -> Vec<(usize, Range<usize>)> {
        let mut placed: Vec<_> = self
            .model
            .iter()
            .map(|ModelMark { mark, .. }| {
                (
                    Arc::as_ptr(&mark.decoration) as usize,
                    mark.start..mark.end(),
                )
            })
            .collect();
        placed.sort_by_key(|(id, bytes)| (*id, bytes.start, bytes.end));
        placed
    }

    /// The model range a cursor at {at} would stop strictly inside, though
    /// it may not.
    fn blocking(&self, at: usize) -> Option<Range<usize>> {
        let cursor = self.buffer.cursor_byte();
        self.model_range_around(at, |mark| {
            let decoration = &mark.decoration;
            let revealed =
                decoration.reveal == Reveal::Cursor && (mark.start..=mark.end()).contains(&cursor);
            !self.raw && (decoration.atomic || decoration.conceal.is_some() && !revealed)
        })
    }

    /// The first model range among the marks {keep} takes that has {at}
    /// strictly inside.
    fn model_range_around(&self, at: usize, keep: impl Fn(&Mark) -> bool) -> Option<Range<usize>> {
        self.model
            .iter()
            .map(|model| &model.mark)
            .filter(|mark| keep(mark))
            .map(|mark| mark.start..mark.end())
            .find(|bytes| bytes.start < at && at < bytes.end)
    }

    fn assert_same_selection(&self, before: &Snapshot) {
        let now = self.snapshot();
        assert_eq!(
            (&now.text, now.cursor),
            (&before.text, before.cursor),
            "{}",
            self.context()
        );
        if !now.any_atomic && !before.any_atomic {
            assert_eq!(now.anchor, before.anchor, "{}", self.context());
        }
    }

    /// Whether a mark, placed or from a rule, that is atomic or conceals has
    /// {at} strictly inside, which the cursor may be moved out of.
    fn may_not_stop_at(&self, at: usize) -> bool {
        self.buffer.touching(at..=at).any(|mark| {
            let decoration = mark.decoration;
            (decoration.atomic || decoration.conceal.is_some())
                && mark.bytes.start < at
                && at < mark.bytes.end
        })
    }

    fn atomic_around(&self, at: usize) -> Option<Range<usize>> {
        self.model_range_around(at, |mark| !self.raw && mark.decoration.atomic)
    }

    fn check(&self, op: Op) {
        let buf = &self.buffer;
        let text = buf.value();
        let lines: Vec<&str> = (0..buf.line_count()).map(|y| buf.line(y)).collect();
        assert_eq!(
            lines,
            text.split('\n').collect::<Vec<_>>(),
            "the line index drifted\n{}",
            self.context()
        );
        let mut y = 0;
        for (at, c) in text.char_indices().chain([(text.len(), '\0')]) {
            assert_eq!(buf.line_of(at), y, "line_of({at})\n{}", self.context());
            y += usize::from(c == '\n');
        }

        let cursor = buf.cursor_byte();
        assert!(
            text.is_char_boundary(cursor),
            "the cursor splits a char\n{}",
            self.context()
        );
        assert_eq!(
            buf.line_of(cursor),
            buf.y(),
            "the cursor line is off\n{}",
            self.context()
        );
        if let Some(anchor) = buf.anchor() {
            assert!(
                text.is_char_boundary(anchor) && anchor != cursor,
                "a bad anchor\n{}",
                self.context()
            );
        }

        assert_eq!(
            self.placed(),
            self.expected_placed(),
            "the marks moved unlike the model\n{}",
            self.context()
        );

        if let Some(blocked) = self.blocking(cursor) {
            panic!(
                "the cursor stopped inside {blocked:?} after {op:?}\n{}",
                self.context()
            );
        }
        // Only a fresh selection: an anchor set earlier stays put when a mark
        // placed since lands around it.
        if op == Op::Select
            && let Some(anchor) = buf.anchor()
            && let Some(blocked) = self.atomic_around(anchor)
        {
            panic!(
                "the anchor stopped inside atomic {blocked:?}\n{}",
                self.context()
            );
        }
    }
}

#[test]
fn random_ops_match_the_models() {
    for seed in seeds(SEEDS) {
        let mut driver = Driver::new(seed);
        for _ in 0..OPS_PER_SEED {
            driver.step();
        }
    }
}
