//! Vim-style modal editing for the chat input.
//!
//! Insert mode is the regular editor, so there this layer only answers `Esc`.
//! In normal mode a key reads the value once, works out offsets with the pure
//! functions in [`motion`] and [`edit`], and writes the result back.
//! [`TextBuffer`] stays the only owner of the text and the cursor.
//!
//! Nothing here knows the app, the theme or Lua, so the module can move to a
//! crate of its own once something else needs it.

mod edit;
mod motion;
mod undo;

use std::mem;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::text_buffer::TextBuffer;
use edit::{Operator, Region, Register};
use motion::Motion;
use undo::{Snapshot, UndoHistory};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VimMode {
    Normal,
    Insert,
}

impl VimMode {
    /// The spelling plugins read.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Insert => "insert",
        }
    }
}

/// What the input box does with a key once vim has seen it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VimOutcome {
    /// Not a vim key, so the box runs its usual path.
    Unhandled,
    /// Vim used the key, which may have changed the text, the cursor or the
    /// mode.
    Handled,
    /// `Enter` in normal mode, where a trailing backslash is just text.
    Submit,
    /// `k` on the first line.
    HistoryPrev,
    /// `j` on the last line.
    HistoryNext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    /// The first `g` of `gg`.
    G,
    Operator(Operator),
    /// An operator waiting for the second `g` of `gg`.
    OperatorG(Operator),
}

/// Where an edit the host makes outside a key started. In normal mode the
/// edit lands as one undo step, and in insert mode it joins the session in
/// progress.
pub(crate) struct EditStart(Option<(Snapshot, u64)>);

pub(crate) struct Vim {
    mode: VimMode,
    pending: Pending,
    /// The column `j` and `k` aim for, kept across lines too short for it.
    want_col: Option<usize>,
    register: Register,
    undo: UndoHistory,
}

impl Vim {
    /// Starts in insert mode, so turning vim on never swallows what the user
    /// types next.
    pub(crate) fn new(buf: &TextBuffer) -> Self {
        let mut vim = Self {
            mode: VimMode::Insert,
            pending: Pending::None,
            want_col: None,
            register: Register::default(),
            undo: UndoHistory::default(),
        };
        vim.reset(buf);
        vim
    }

    pub(crate) fn mode(&self) -> VimMode {
        self.mode
    }

    /// Whether `Esc` is a vim key right now, leaving insert mode or cancelling
    /// a half-typed command. Otherwise it belongs to the app.
    pub(crate) fn wants_esc(&self) -> bool {
        self.mode == VimMode::Insert || self.pending != Pending::None
    }

    /// The half-typed command, empty when there is none.
    pub(crate) fn pending_label(&self) -> &'static str {
        match self.pending {
            Pending::None => "",
            Pending::G => "g",
            Pending::Operator(op) => op.label(),
            Pending::OperatorG(op) => op.label_with_g(),
        }
    }

    /// A fresh draft: insert mode, nothing half-typed, and no undo steps left
    /// from the text before. The register stays, so a yank outlives a submit.
    pub(crate) fn reset(&mut self, buf: &TextBuffer) {
        self.mode = VimMode::Insert;
        self.forget(buf);
    }

    /// The host swapped the whole value: a history entry, `$EDITOR`, a draft
    /// or a rewind. The undo steps belong to the text that left, and the mode
    /// stays.
    pub(crate) fn text_replaced(&mut self, buf: &mut TextBuffer) {
        self.forget(buf);
        self.clamp_cursor(buf);
    }

    fn forget(&mut self, buf: &TextBuffer) {
        self.pending = Pending::None;
        self.want_col = None;
        self.undo.clear();
        if self.mode == VimMode::Insert {
            self.undo.open(Snapshot::of(buf), buf.version());
        }
    }

    /// The host put the cursor somewhere of its own accord, a click for one.
    pub(crate) fn cursor_moved(&mut self, buf: &mut TextBuffer) {
        self.pending = Pending::None;
        self.want_col = None;
        self.clamp_cursor(buf);
    }

    pub(crate) fn begin_edit(&self, buf: &TextBuffer) -> EditStart {
        EditStart((self.mode == VimMode::Normal).then(|| (Snapshot::of(buf), buf.version())))
    }

    pub(crate) fn end_edit(&mut self, start: EditStart, buf: &mut TextBuffer) {
        if let Some((before, version)) = start.0
            && buf.version() != version
        {
            self.undo.push(before);
        }
        self.cursor_moved(buf);
    }

    /// In normal mode the cursor sits on a character, never past the last one
    /// of its line.
    fn clamp_cursor(&self, buf: &mut TextBuffer) {
        if self.mode != VimMode::Normal {
            return;
        }
        let len = buf.lines()[buf.y()].chars().count();
        if len > 0 && buf.x() >= len {
            buf.set_cursor(buf.y(), len - 1);
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent, buf: &mut TextBuffer) -> VimOutcome {
        match self.mode {
            VimMode::Insert => self.insert_key(key, buf),
            VimMode::Normal => self.normal_key(key, buf),
        }
    }

    fn insert_key(&mut self, key: KeyEvent, buf: &mut TextBuffer) -> VimOutcome {
        if key.code != KeyCode::Esc {
            return VimOutcome::Unhandled;
        }
        self.mode = VimMode::Normal;
        self.undo.close(buf.version());
        buf.set_cursor(buf.y(), buf.x().saturating_sub(1));
        VimOutcome::Handled
    }

    fn normal_key(&mut self, key: KeyEvent, buf: &mut TextBuffer) -> VimOutcome {
        let pending = mem::replace(&mut self.pending, Pending::None);
        if key.code == KeyCode::Esc {
            return if pending == Pending::None {
                VimOutcome::Unhandled
            } else {
                VimOutcome::Handled
            };
        }
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('r') {
            self.step(buf, UndoHistory::redo);
            return VimOutcome::Handled;
        }
        if !key.modifiers.is_empty() {
            return VimOutcome::Unhandled;
        }
        match key.code {
            KeyCode::Tab => return VimOutcome::Unhandled,
            KeyCode::Enter => return VimOutcome::Submit,
            KeyCode::Char('u') if pending == Pending::None => {
                self.step(buf, UndoHistory::undo);
                return VimOutcome::Handled;
            }
            _ => {}
        }

        let text = buf.value();
        let at = buf.cursor_byte();
        let version = buf.version();
        let outcome = self.command(pending, key.code, &text, at, buf);
        if self.mode == VimMode::Insert {
            self.want_col = None;
            self.undo.open(Snapshot::new(text, at), version);
        } else if buf.version() != version {
            self.want_col = None;
            self.undo.push(Snapshot::new(text, at));
        }
        self.clamp_cursor(buf);
        outcome
    }

    /// `u` or `Ctrl+R`, whichever way {pick} walks the history.
    fn step(
        &mut self,
        buf: &mut TextBuffer,
        pick: fn(&mut UndoHistory, Snapshot) -> Option<Snapshot>,
    ) {
        if let Some(state) = pick(&mut self.undo, Snapshot::of(buf)) {
            state.restore(buf);
        }
        self.want_col = None;
        self.clamp_cursor(buf);
    }

    fn command(
        &mut self,
        pending: Pending,
        code: KeyCode,
        text: &str,
        at: usize,
        buf: &mut TextBuffer,
    ) -> VimOutcome {
        match pending {
            Pending::None => return self.start(code, text, at, buf),
            Pending::G if code == KeyCode::Char('g') => {
                self.go(Motion::FirstLine, text, at, buf);
            }
            Pending::Operator(op) => self.operator_key(op, code, text, at, buf),
            Pending::OperatorG(op) if code == KeyCode::Char('g') => {
                self.operate(op, Motion::FirstLine, text, at, buf);
            }
            Pending::G | Pending::OperatorG(_) => {}
        }
        VimOutcome::Handled
    }

    /// A key with nothing half-typed before it.
    fn start(&mut self, code: KeyCode, text: &str, at: usize, buf: &mut TextBuffer) -> VimOutcome {
        if let Some(motion) = Motion::from_key(code) {
            return self.go(motion, text, at, buf);
        }
        let c = match code {
            KeyCode::Char(c) => c,
            KeyCode::Delete => 'x',
            _ => return VimOutcome::Handled,
        };
        if let Some(op) = Operator::from_char(c) {
            self.pending = Pending::Operator(op);
            return VimOutcome::Handled;
        }
        match c {
            'g' => self.pending = Pending::G,
            'i' => self.insert_at(at, buf),
            'a' => self.insert_at(motion::after(text, at), buf),
            'I' => self.insert_at(motion::indent_end(text, at), buf),
            'A' => self.insert_at(motion::line_end(text, at), buf),
            'o' => {
                let end = motion::line_end(text, at);
                self.open_line(text, end, end + 1, buf);
            }
            'O' => {
                let start = motion::line_start(text, at);
                self.open_line(text, start, start, buf);
            }
            'x' => self.operate(Operator::Delete, Motion::Right, text, at, buf),
            'X' => self.operate(Operator::Delete, Motion::Left, text, at, buf),
            's' => self.operate(Operator::Change, Motion::Right, text, at, buf),
            'D' => self.operate(Operator::Delete, Motion::LineEnd, text, at, buf),
            'C' => self.operate(Operator::Change, Motion::LineEnd, text, at, buf),
            'S' => self.apply(Operator::Change, Region::lines(text, at, at), at, text, buf),
            'Y' => self.apply(Operator::Yank, Region::lines(text, at, at), at, text, buf),
            'p' | 'P' => {
                if let Some((next, cursor)) = edit::put(text, at, &self.register, c == 'p') {
                    write(buf, next, cursor);
                }
            }
            // The palette and shell input open on the first character, and
            // normal mode types none, so an empty draft lets these two through.
            '/' | '!' if text.is_empty() => {
                self.mode = VimMode::Insert;
                return VimOutcome::Unhandled;
            }
            _ => {}
        }
        VimOutcome::Handled
    }

    fn go(&mut self, motion: Motion, text: &str, at: usize, buf: &mut TextBuffer) -> VimOutcome {
        let col = self.aim(motion, text, at);
        let Some(target) = motion.target(text, at, col) else {
            return match motion {
                Motion::Up => VimOutcome::HistoryPrev,
                Motion::Down => VimOutcome::HistoryNext,
                _ => VimOutcome::Handled,
            };
        };
        move_cursor(buf, target);
        VimOutcome::Handled
    }

    /// The column `j` and `k` aim for. A run of vertical moves keeps the one
    /// the first of them started from, `$` aims past every line end, and any
    /// other motion forgets it.
    fn aim(&mut self, motion: Motion, text: &str, at: usize) -> usize {
        let col = match motion {
            Motion::Up | Motion::Down => self.want_col.unwrap_or_else(|| motion::column(text, at)),
            Motion::LineEnd => usize::MAX,
            _ => {
                self.want_col = None;
                return 0;
            }
        };
        self.want_col = Some(col);
        col
    }

    fn operator_key(
        &mut self,
        op: Operator,
        code: KeyCode,
        text: &str,
        at: usize,
        buf: &mut TextBuffer,
    ) {
        match code {
            KeyCode::Char('g') => self.pending = Pending::OperatorG(op),
            KeyCode::Char(c) if Operator::from_char(c) == Some(op) => {
                self.apply(op, Region::lines(text, at, at), at, text, buf);
            }
            _ => {
                if let Some(motion) = Motion::from_key(code) {
                    self.operate(op, motion, text, at, buf);
                }
            }
        }
    }

    fn operate(
        &mut self,
        op: Operator,
        motion: Motion,
        text: &str,
        at: usize,
        buf: &mut TextBuffer,
    ) {
        let col = self.aim(motion, text, at);
        if motion == Motion::WordForward {
            self.apply(op, Region::of_word(text, at, op), at, text, buf);
            return;
        }
        if let Some(target) = motion.target(text, at, col) {
            let region = Region::of_motion(text, at, motion, target, op);
            self.apply(op, region, at.min(target), text, buf);
        }
    }

    /// Runs {op} over {region}. A yank leaves the cursor on {landing}, the
    /// start of what the motion covered, and a change starts typing there.
    fn apply(
        &mut self,
        op: Operator,
        region: Region,
        landing: usize,
        text: &str,
        buf: &mut TextBuffer,
    ) {
        if region.is_empty() {
            if op == Operator::Change {
                self.insert_at(landing, buf);
            }
            return;
        }
        self.register = region.yank(text);
        match op {
            Operator::Yank => move_cursor(buf, landing),
            Operator::Delete => {
                let removed = region.deleted(text);
                let Some(next) = edit::splice(text, removed.clone(), "") else {
                    return;
                };
                let cursor = match region {
                    Region::Lines(_) => motion::first_non_blank(&next, removed.start),
                    Region::Chars(_) => removed.start,
                };
                write(buf, next, cursor);
            }
            Operator::Change => {
                let changed = region.changed();
                let Some(next) = edit::splice(text, changed.clone(), "") else {
                    return;
                };
                write(buf, next, changed.start);
                self.mode = VimMode::Insert;
            }
        }
    }

    fn insert_at(&mut self, at: usize, buf: &mut TextBuffer) {
        self.mode = VimMode::Insert;
        move_cursor(buf, at);
    }

    /// `o` and `O`: a newline at {at}, then insert mode on {cursor}, the empty
    /// line it made.
    fn open_line(&mut self, text: &str, at: usize, cursor: usize, buf: &mut TextBuffer) {
        if let Some(next) = edit::splice(text, at..at, "\n") {
            write(buf, next, cursor);
            self.mode = VimMode::Insert;
        }
    }
}

/// Writes {text} verbatim, the way it was read: what vim puts back came out of
/// this same value, so the paste cleanup a plugin's write gets would only
/// change it.
fn write(buf: &mut TextBuffer, text: String, cursor: usize) {
    buf.set_value(text);
    move_cursor(buf, cursor);
}

/// Every offset here was stepped to one character at a time, so a refusal is
/// a bug in this module. It is logged, not fatal.
fn move_cursor(buf: &mut TextBuffer, at: usize) {
    if let Err(error) = buf.set_cursor_byte(at) {
        tracing::warn!(error, at, len = buf.byte_len(), "vim cursor offset refused");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::motion::tests::{mark, parse};
    use super::*;
    use maki_lua::Key;
    use test_case::test_case;

    /// Reads vim notation, `d<Esc>w` being `d`, `Esc` and `w`.
    pub(crate) fn keys(notation: &str) -> Vec<KeyEvent> {
        let mut out = Vec::new();
        let mut rest = notation;
        while let Some(c) = rest.chars().next() {
            let len = match c {
                '<' => rest.find('>').map_or(1, |i| i + 1),
                _ => c.len_utf8(),
            };
            out.push(Key::parse(&rest[..len]).unwrap().into());
            rest = &rest[len..];
        }
        out
    }

    fn shown(buf: &TextBuffer) -> String {
        mark(&buf.value(), buf.cursor_byte())
    }

    /// What the input box does: a key vim leaves alone goes to the editor,
    /// as one undo step when it lands in normal mode.
    fn press(vim: &mut Vim, buf: &mut TextBuffer, key: KeyEvent) -> VimOutcome {
        let outcome = vim.handle_key(key, buf);
        if outcome == VimOutcome::Unhandled {
            let start = vim.begin_edit(buf);
            buf.handle_key(key);
            vim.end_edit(start, buf);
        }
        outcome
    }

    /// {marked} in normal mode, as if `Esc` had just landed there.
    fn normal(marked: &str) -> (Vim, TextBuffer) {
        let (text, at) = parse(marked);
        let mut buf = TextBuffer::new(text);
        buf.set_cursor_byte(at).unwrap();
        let mut vim = Vim::new(&buf);
        vim.mode = VimMode::Normal;
        vim.undo.clear();
        (vim, buf)
    }

    fn run(marked: &str, notation: &str) -> (Vim, TextBuffer) {
        let (mut vim, mut buf) = normal(marked);
        for key in keys(notation) {
            press(&mut vim, &mut buf, key);
        }
        (vim, buf)
    }

    fn typed(marked: &str, notation: &str) -> String {
        shown(&run(marked, notation).1)
    }

    /// The same, for keys that must leave normal mode in charge.
    fn typed_in_normal_mode(marked: &str, notation: &str) -> String {
        let (vim, buf) = run(marked, notation);
        assert_eq!(vim.mode(), VimMode::Normal, "{notation} left normal mode");
        shown(&buf)
    }

    #[test_case("hel|lo world",       "w",    "hello |world"       ; "w_goes_to_the_next_word")]
    #[test_case("hello |world",       "b",    "|hello world"       ; "b_goes_to_the_previous_word")]
    #[test_case("h|ello world",       "e",    "hell|o world"       ; "e_goes_to_the_word_end")]
    #[test_case("hello wo|rld",       "w",    "hello worl|d"       ; "w_past_the_last_word_stays_on_the_text")]
    #[test_case("ab|c",               "l",    "ab|c"               ; "l_stops_on_the_last_char")]
    #[test_case("|abc",               "h",    "|abc"               ; "h_stops_at_the_line_start")]
    #[test_case("  a|bc",             "0",    "|  abc"             ; "zero")]
    #[test_case("  a|bc",             "^",    "  |abc"             ; "caret")]
    #[test_case("|abc",               "$",    "ab|c"               ; "dollar")]
    #[test_case("|abc",               "<End>", "ab|c"              ; "end_key")]
    #[test_case("ab|c",               "<Home>", "|abc"             ; "home_key")]
    #[test_case("a\nb\n c|d",         "gg",   "|a\nb\n cd"         ; "gg")]
    #[test_case("|a\nb\n cd",         "G",    "a\nb\n |cd"         ; "capital_g")]
    #[test_case("abc|d\nxy",          "j",    "abcd\nx|y"          ; "j_stops_on_the_last_char_of_a_shorter_line")]
    #[test_case("abc|d\nxy\nabcd",    "jj",   "abcd\nxy\nabc|d"    ; "j_keeps_the_wanted_column")]
    #[test_case("|ab\nxyz",           "$j",   "ab\nxy|z"           ; "dollar_then_j_stays_at_the_line_end")]
    #[test_case("ab\nx|y",            "<Up>", "a|b\nxy"            ; "up_arrow")]
    #[test_case("a|b",                "g<Esc>g", "a|b"             ; "esc_cancels_a_lone_g")]
    #[test_case("a|b",                "qzr.", "a|b"                ; "unknown_keys_change_nothing")]
    fn normal_mode_motion(start: &str, notation: &str, expected: &str) {
        assert_eq!(typed(start, notation), expected);
    }

    #[test_case("ab|c",   "i",  "ab|c"     ; "i_inserts_before_the_cursor")]
    #[test_case("ab|c",   "a",  "abc|"     ; "a_inserts_after_it")]
    #[test_case("  a|b",  "I",  "  |ab"    ; "capital_i_inserts_at_the_first_non_blank")]
    #[test_case("a|b\nc", "A",  "ab|\nc"   ; "capital_a_inserts_at_the_line_end")]
    #[test_case("a|b\nc", "o",  "ab\n|\nc" ; "o_opens_a_line_below")]
    #[test_case("a\nb|c", "O",  "a\n|\nbc" ; "capital_o_opens_a_line_above")]
    #[test_case("|",      "a",  "|"        ; "a_on_an_empty_line")]
    fn insert_keys_put_the_cursor(start: &str, notation: &str, expected: &str) {
        let (vim, buf) = run(start, notation);
        assert_eq!(vim.mode(), VimMode::Insert);
        assert_eq!(shown(&buf), expected);
    }

    #[test_case("abc|",    "ab|c"    ; "moves_one_left_from_the_end")]
    #[test_case("a|bc",    "|abc"    ; "moves_one_left_inside_the_line")]
    #[test_case("|abc",    "|abc"    ; "stays_at_the_line_start")]
    #[test_case("a\n|bc", "a\n|bc" ; "does_not_wrap_to_the_line_above")]
    fn esc_leaves_insert_mode(start: &str, expected: &str) {
        let (text, at) = parse(start);
        let mut buf = TextBuffer::new(text);
        buf.set_cursor_byte(at).unwrap();
        let mut vim = Vim::new(&buf);
        assert_eq!(
            vim.handle_key(keys("<Esc>")[0], &mut buf),
            VimOutcome::Handled
        );
        assert_eq!(vim.mode(), VimMode::Normal);
        assert_eq!(shown(&buf), expected);
    }

    #[test]
    fn esc_wants_and_releases() {
        let (mut vim, mut buf) = normal("a|b");
        assert!(!vim.wants_esc(), "idle normal mode leaves Esc to the app");
        press(&mut vim, &mut buf, keys("g")[0]);
        assert!(vim.wants_esc(), "a half-typed command is cancelled by it");
        press(&mut vim, &mut buf, keys("i")[0]);
        assert!(!vim.wants_esc());
        press(&mut vim, &mut buf, keys("i")[0]);
        assert!(vim.wants_esc(), "insert mode leaves on it");
    }

    #[test_case("a|b",     "k",       VimOutcome::HistoryPrev ; "k_on_the_first_line")]
    #[test_case("a\nb|c",  "j",       VimOutcome::HistoryNext ; "j_on_the_last_line")]
    #[test_case("a\nb|c",  "<Down>",  VimOutcome::HistoryNext ; "down_on_the_last_line")]
    #[test_case("a|b\nc",  "j",       VimOutcome::Handled     ; "j_with_a_line_below")]
    #[test_case("a|b",     "<CR>",    VimOutcome::Submit      ; "enter_submits")]
    #[test_case("a|b",     "<Tab>",   VimOutcome::Unhandled   ; "tab_is_the_apps")]
    #[test_case("a|b",     "<Esc>",   VimOutcome::Unhandled   ; "an_idle_esc_is_the_apps")]
    #[test_case("a|b",     "<C-w>",   VimOutcome::Unhandled   ; "ctrl_keys_keep_their_meaning")]
    #[test_case("|",       "/",       VimOutcome::Unhandled   ; "slash_on_an_empty_draft_is_typed")]
    #[test_case("a|b",     "/",       VimOutcome::Handled     ; "slash_with_text_does_nothing")]
    fn normal_mode_hands_back(start: &str, notation: &str, expected: VimOutcome) {
        let (mut vim, mut buf) = normal(start);
        assert_eq!(vim.handle_key(keys(notation)[0], &mut buf), expected);
    }

    #[test]
    fn dk_on_the_first_line_is_not_history() {
        let (mut vim, mut buf) = normal("a|b");
        press(&mut vim, &mut buf, keys("d")[0]);
        assert_eq!(vim.handle_key(keys("k")[0], &mut buf), VimOutcome::Handled);
        assert_eq!(shown(&buf), "a|b");
    }

    #[test]
    fn slash_on_an_empty_draft_types_and_enters_insert_mode() {
        let (vim, buf) = run("|", "/");
        assert_eq!(vim.mode(), VimMode::Insert);
        assert_eq!(buf.value(), "/");
    }

    #[test_case("ab|c",          "x",     "a|b"             ; "x_at_the_line_end")]
    #[test_case("a\n|\nb",       "x",     "a\n|\nb"         ; "x_on_an_empty_line")]
    #[test_case("|abc",          "X",     "|abc"            ; "capital_x_at_the_line_start")]
    #[test_case("a|bc",          "X",     "|bc"             ; "capital_x")]
    #[test_case("a|bc",          "<Del>", "a|c"             ; "delete_key")]
    #[test_case("|foo bar",      "dw",    "|bar"            ; "dw")]
    #[test_case("foo b|ar\nbaz", "dw",    "foo |b\nbaz"     ; "dw_on_the_last_word_keeps_the_newline")]
    #[test_case("|foo bar",      "de",    "| bar"           ; "de")]
    #[test_case("foo b|ar",      "db",    "foo |ar"         ; "db")]
    #[test_case("  fo|o",        "d0",    "|o"              ; "d0")]
    #[test_case("  fo|o",        "d^",    "  |o"            ; "d_caret")]
    #[test_case("a|bc\nd",       "d$",    "|a\nd"           ; "d_dollar")]
    #[test_case("a|bc\nd",       "D",     "|a\nd"           ; "capital_d")]
    #[test_case("a|b\ncd\ne",    "dd",    "|cd\ne"          ; "dd_on_the_first_line")]
    #[test_case("a\nc|d\ne",     "dd",    "a\n|e"           ; "dd_on_a_middle_line")]
    #[test_case("a\n  c|d",      "dd",    "|a"              ; "dd_on_the_last_line")]
    #[test_case("a|b",           "dd",    "|"               ; "dd_on_the_only_line")]
    #[test_case("a|b\ncd\ne",    "dj",    "|e"              ; "dj_is_linewise")]
    #[test_case("a\nb|c\ne",     "dk",    "|e"              ; "dk_is_linewise")]
    #[test_case("a\nb\nc|d",     "dgg",   "|"               ; "dgg_is_linewise")]
    #[test_case("a\nb|c\ne",     "dG",    "|a"              ; "capital_dg_is_linewise")]
    #[test_case("a|b",           "dx",    "a|b"             ; "an_unknown_motion_cancels")]
    #[test_case("a|b",           "d<Esc>x", "|a"            ; "esc_cancels_the_operator")]
    fn deleting(start: &str, notation: &str, expected: &str) {
        assert_eq!(typed_in_normal_mode(start, notation), expected);
    }

    #[test_case("|foo bar",  "cwX",  "X| bar"    ; "cw_acts_like_ce")]
    #[test_case("foo|  bar", "cwX",  "fooX|bar"  ; "cw_on_a_blank_deletes_the_blanks")]
    #[test_case("a\n b|c\nd", "ccX", "a\nX|\nd"  ; "cc_clears_the_line")]
    #[test_case("a\n b|c\nd", "SX",  "a\nX|\nd"  ; "capital_s_clears_the_line")]
    #[test_case("a|bc",      "CX",   "aX|"       ; "capital_c")]
    #[test_case("a|bc",      "sX",   "aX|c"      ; "s")]
    #[test_case("|",         "sX",   "X|"        ; "s_on_an_empty_line")]
    #[test_case("|abc",      "chX",  "X|abc"     ; "ch_at_the_line_start")]
    #[test_case("|\na",      "clX",  "X|\na"     ; "cl_on_an_empty_line")]
    #[test_case("|\na",      "c$X",  "X|\na"     ; "c_dollar_on_an_empty_line")]
    #[test_case("a\n|",      "cwX",  "a\nX|"     ; "cw_on_an_empty_last_line")]
    fn changing(start: &str, notation: &str, expected: &str) {
        let (vim, buf) = run(start, notation);
        assert_eq!(vim.mode(), VimMode::Insert);
        assert_eq!(shown(&buf), expected);
    }

    #[test_case("a|b\nc",    "yyp",   "ab\n|ab\nc"    ; "yy_then_p_puts_the_line_below")]
    #[test_case("a\nb|c",    "YP",    "a\n|bc\nbc"    ; "capital_y_then_capital_p_puts_it_above")]
    #[test_case("|foo bar",  "ywP",   "foo| foo bar"  ; "yw_then_capital_p_puts_before")]
    #[test_case("|foo bar",  "yw$p",  "foo barfoo| "  ; "yw_then_p_puts_after")]
    #[test_case("foo |bar",  "yb",    "|foo bar"      ; "yb_moves_to_the_start")]
    #[test_case("a\nb|c",    "yk",    "|a\nbc"        ; "yk_moves_up")]
    #[test_case("a|bc",      "p",     "a|bc"          ; "p_with_an_empty_register")]
    #[test_case("a|bc",      "xp",    "ac|b"          ; "xp_swaps_two_chars")]
    #[test_case("a|b\nc",    "ddp",   "c\n|ab"        ; "ddp_swaps_two_lines")]
    fn yanking_and_putting(start: &str, notation: &str, expected: &str) {
        assert_eq!(typed_in_normal_mode(start, notation), expected);
    }

    /// A failed motion drops the operator, so the next key is a normal mode
    /// command again: here `x`, which deletes rather than gets typed. What
    /// vim 9.1 gives for the same keys typed one by one.
    #[test_case("|abc",     "cbx", "|bc"     ; "cb_at_the_start_of_the_text")]
    #[test_case("|abc",     "dbx", "|bc"     ; "db_at_the_start_of_the_text")]
    #[test_case("abc\n|d",  "cjx", "abc\n|"  ; "cj_on_the_last_line")]
    #[test_case("a|bc\nd",  "ckx", "a|c\nd"  ; "ck_on_the_first_line")]
    fn a_failed_motion_cancels_the_operator(start: &str, notation: &str, expected: &str) {
        assert_eq!(typed_in_normal_mode(start, notation), expected);
    }

    #[test_case("|abc"                    ; "one_word")]
    #[test_case("|abc\n\ndef"              ; "an_empty_line_between_words")]
    #[test_case("|foo\nbar baz"            ; "several_words_across_lines")]
    #[test_case("|  abc\n  def\n  ghi"     ; "indented_lines")]
    #[test_case("|abc\n   \ndef"           ; "a_blank_line_between_words")]
    #[test_case("|abc\n\n"                 ; "trailing_empty_lines")]
    #[test_case("|a\nb\nc"                 ; "single_character_lines")]
    #[test_case("|a.b, c!"                 ; "punctuation")]
    #[test_case("|  abc"                   ; "leading_blanks")]
    #[test_case("|é漢\nç好"                 ; "multibyte_characters")]
    fn cb_at_the_start_of_the_text_changes_nothing(start: &str) {
        let (mut vim, mut buf) = normal(start);
        let version = buf.version();
        for key in keys("cbZ<Esc>") {
            press(&mut vim, &mut buf, key);
        }
        assert_eq!(vim.mode(), VimMode::Normal);
        assert_eq!(vim.pending_label(), "");
        assert_eq!(buf.version(), version);
        assert_eq!(shown(&buf), start);
    }

    /// What vim 9.1 gives for the same keys: the text, the cursor, and what the
    /// register holds. A range that crosses lines is where a naive byte range
    /// would join lines or keep a stray indent.
    #[test_case("  abc\n|  def",   "db",       "  |def",        "  abc", true  ; "db_from_column_0_deletes_the_indented_line_above")]
    #[test_case("abc\n\n|def",     "db",       "abc\n|def",     "",      true  ; "db_from_column_0_deletes_the_empty_line_above")]
    #[test_case("foo bar\n|  baz", "db",       "foo| \n  baz",  "bar",   false ; "db_from_column_0_keeps_the_line_break")]
    #[test_case("  abc\n|  def",   "cbZ<Esc>", "|Z\n  def",     "  abc", true  ; "cb_from_column_0_changes_the_indented_line_above")]
    #[test_case("abc\n\n|def",     "cbZ<Esc>", "abc\n|Z\ndef",  "",      true  ; "cb_onto_an_empty_line_keeps_both_line_breaks")]
    #[test_case("  abc\n|  def",   "yb",       "  |abc\n  def", "  abc", true  ; "yb_from_column_0_yanks_the_line_above")]
    #[test_case("abc\n|\ndef",     "de",       "|abc",          "\ndef", true  ; "de_from_an_empty_line_leaves_no_blank_line")]
    #[test_case("a b\n | \nc",     "de",       "|a b",          "  \nc", true  ; "de_from_a_blank_line_deletes_both_lines")]
    #[test_case("abc\n|\ndef x",   "de",       "abc\n| x",      "\ndef", false ; "de_that_leaves_text_stays_charwise")]
    #[test_case("abc\n|\ndef",          "cbZ<Esc>", "|Z\n\ndef",          "abc",      true ; "cb_onto_an_empty_line_keeps_the_line_below")]
    #[test_case("foo\n|bar baz",        "cbZ<Esc>", "|Z\nbar baz",        "foo",      true ; "cb_from_column_0_changes_the_unindented_line_above")]
    #[test_case("  abc\n|  def\n  ghi", "db",       "  |def\n  ghi",      "  abc",    true ; "db_preserves_the_lines_below_an_indented_line")]
    #[test_case("  abc\n|  def\n  ghi", "cbZ<Esc>", "|Z\n  def\n  ghi",   "  abc",    true ; "cb_preserves_the_lines_below_an_indented_line")]
    #[test_case("  abc\n  def\n|  ghi", "db",       "  abc\n  |ghi",      "  def",    true ; "db_preserves_the_lines_above_an_indented_line")]
    #[test_case("  abc\n  def\n|  ghi", "cbZ<Esc>", "  abc\n|Z\n  ghi",   "  def",    true ; "cb_preserves_the_lines_above_an_indented_line")]
    #[test_case("abc\n|   \ndef",       "de",       "|abc",               "   \ndef", true ; "de_from_the_start_of_a_blank_line_removes_its_indent")]
    #[test_case("abc\n |  \ndef",       "de",       "|abc",               "   \ndef", true ; "de_from_inside_a_blank_line_removes_its_indent")]
    #[test_case("abc\n  | \ndef",       "de",       "|abc",               "   \ndef", true ; "de_from_the_end_of_a_blank_line_removes_its_indent")]
    #[test_case("abc\n|   \ndef",       "db",       "  | \ndef",          "abc",      true ; "db_onto_a_blank_line_clamps_the_cursor_to_its_last_character")]
    #[test_case("abc\n |  \ndef",       "db",       "|def",               "abc\n   ", true ; "db_from_inside_a_blank_line_deletes_both_lines")]
    #[test_case("abc\n  | \ndef",       "db",       "|def",               "abc\n   ", true ; "db_from_the_end_of_a_blank_line_deletes_both_lines")]
    #[test_case("abc\n|   \ndef",       "cbZ<Esc>", "|Z\n   \ndef",       "abc",      true ; "cb_onto_a_blank_line_keeps_its_indent")]
    #[test_case("abc\n   \n|def",       "cbZ<Esc>", "|Z\ndef",            "abc\n   ", true ; "cb_across_a_blank_line_changes_both_lines")]
    #[test_case("abc\n|\n",             "de",       "|abc",               "\n",       true ; "de_removes_trailing_empty_lines")]
    #[test_case("abc\n|\n",             "cbZ<Esc>", "|Z\n\n",             "abc",      true ; "cb_onto_a_trailing_empty_line_keeps_both_line_breaks")]
    #[test_case("|a\nb\nc",             "de",       "|c",                 "a\nb",     true ; "de_from_a_single_character_line_deletes_both_lines")]
    #[test_case("a\n|b\nc",             "de",       "|a",                 "b\nc",     true ; "de_through_a_single_character_last_line_keeps_no_empty_line")]
    #[test_case("a\n|b\nc",             "cbZ<Esc>", "|Z\nb\nc",           "a",        true ; "cb_from_a_single_character_middle_line_keeps_the_lines_below")]
    #[test_case("a\nb\n|c",             "cbZ<Esc>", "a\n|Z\nc",           "b",        true ; "cb_from_a_single_character_last_line_keeps_the_line_above")]
    fn an_operator_across_lines(
        start: &str,
        notation: &str,
        expected: &str,
        register: &str,
        linewise: bool,
    ) {
        let (vim, buf) = run(start, notation);
        assert_eq!(vim.mode(), VimMode::Normal);
        assert_eq!(shown(&buf), expected);
        assert_eq!(
            vim.register,
            Register {
                text: register.into(),
                linewise,
            }
        );
    }

    #[test_case("d",   "d"  ; "an_operator")]
    #[test_case("g",   "g"  ; "a_g")]
    #[test_case("dg",  "dg" ; "an_operator_and_g")]
    #[test_case("dw",  ""   ; "a_finished_command")]
    #[test_case("d<Esc>", "" ; "a_cancelled_command")]
    fn the_half_typed_command_shows(notation: &str, expected: &str) {
        let (vim, _) = run("a|b c", notation);
        assert_eq!(vim.pending_label(), expected);
    }

    #[test_case("|old",       "Anew<Esc>u",     "|old"        ; "one_insert_session_is_one_step")]
    #[test_case("|foo bar",   "cwbaz<Esc>u",    "|foo bar"    ; "a_change_and_its_typing_are_one_step")]
    #[test_case("a|b\nc",     "ddu<C-r>",       "|c"          ; "redo_deletes_the_line_again")]
    #[test_case("a|bc",       "xxuu",           "a|bc"        ; "each_command_is_a_step")]
    #[test_case("a|bc",       "xu<C-r><C-r>",   "a|c"         ; "redo_past_the_last_step_does_nothing")]
    #[test_case("a|bc",       "u",              "a|bc"        ; "undo_with_nothing_to_undo")]
    #[test_case("a|bc",       "Ax<Esc>xuu",     "a|bc"        ; "an_insert_session_after_a_command")]
    #[test_case("a|bc",       "i<Esc>u",        "|abc"        ; "an_insert_session_that_typed_nothing_is_no_step")]
    fn undoing(start: &str, notation: &str, expected: &str) {
        assert_eq!(typed(start, notation), expected);
    }

    #[test]
    fn a_new_change_after_undo_clears_redo() {
        assert_eq!(typed("a|bc", "xuX<C-r>"), "|bc");
    }

    #[test]
    fn a_host_edit_in_normal_mode_is_one_step() {
        let (mut vim, mut buf) = normal("a|b");
        let start = vim.begin_edit(&buf);
        buf.insert_text("pasted");
        vim.end_edit(start, &mut buf);
        assert_eq!(buf.value(), "apastedb");

        press(&mut vim, &mut buf, keys("u")[0]);
        assert_eq!(shown(&buf), "a|b");
    }

    #[test]
    fn a_host_edit_in_insert_mode_joins_the_session() {
        let (mut vim, mut buf) = run("|", "i");
        buf.insert_text("typed");
        let start = vim.begin_edit(&buf);
        buf.insert_text(" written");
        vim.end_edit(start, &mut buf);
        press(&mut vim, &mut buf, keys("<Esc>")[0]);

        press(&mut vim, &mut buf, keys("u")[0]);
        assert_eq!(buf.value(), "", "one step takes the typing and the edit");
    }

    #[test]
    fn replaced_text_forgets_the_undo_steps_and_keeps_the_mode() {
        let (mut vim, mut buf) = run("a|bc", "x");
        buf.set_value("recalled".into());
        buf.move_to_end();
        vim.text_replaced(&mut buf);
        assert_eq!(vim.mode(), VimMode::Normal);

        press(&mut vim, &mut buf, keys("u")[0]);
        assert_eq!(buf.value(), "recalled");
    }

    #[test]
    fn a_reset_starts_a_fresh_draft_in_insert_mode() {
        let (mut vim, mut buf) = run("a|bc", "yyd");
        buf.clear();
        vim.reset(&buf);
        assert_eq!(vim.mode(), VimMode::Insert);
        assert_eq!(vim.pending_label(), "");

        for key in keys("<Esc>p") {
            press(&mut vim, &mut buf, key);
        }
        assert_eq!(buf.value(), "\nabc", "the register outlives the draft");
    }
}
