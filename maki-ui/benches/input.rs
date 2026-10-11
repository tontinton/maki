//! What a long prompt costs the input box, driven through the same `App`
//! entry points the event loop uses.

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, BenchmarkId, Criterion, criterion_group, criterion_main};
use crossterm::event::{KeyCode, KeyEvent};
use maki_storage::StateDir;
use maki_ui::app::{App, Msg};
use maki_ui::bench_support;
use maki_ui::keybindings::key;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use tempfile::TempDir;

const SIZES: &[usize] = &[1_000, 10_000, 50_000];
const PASTE_LINES: usize = 100;
const SINGLE_LINE_BYTES: usize = 1 << 20;
const MIDDLE_COLUMN: usize = SINGLE_LINE_BYTES / 2;
const WORD: &str = "lorem ";
const UNBROKEN: &str = "x";
const AREA: Rect = Rect::new(0, 0, 120, 40);
const SAMPLES: usize = 10;
const TYPED: char = 'x';
const UNDO: KeyEvent = KeyEvent::new(key::UNDO.code, key::UNDO.modifiers);

/// A bit of everything a prompt tends to hold. The quote is wider than the
/// terminal, so wrapping gets measured too.
const MARKDOWN: &[&str] = &[
    "## Why the input box slows down",
    "",
    "A long prompt has **every** kind of line: headings, lists, code, tables and quotes.",
    "",
    "- a list item with `inline code`",
    "- another item, a little longer than the first one",
    "  - nested item with a [link](https://example.com) and _emphasis_",
    "",
    "```rust",
    "fn total_visual_lines(buffer: &TextBuffer, ew: usize) -> usize {",
    "    buffer.lines().iter().map(|l| wrapped_row_count(l, ew, false)).sum()",
    "}",
    "```",
    "",
    "| case | lines | note |",
    "|---|---|---|",
    "| paste | 50000 | a long prompt |",
    "",
    "> A quoted line long enough to wrap at a hundred and twenty columns, which is how wide the bench draws the terminal, so wrapping is measured as well.",
    "",
];

fn markdown(lines: usize) -> String {
    MARKDOWN
        .iter()
        .cycle()
        .take(lines)
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line of {bytes} bytes with no newline in it, made of {piece}.
fn single_line(bytes: usize, piece: &str) -> String {
    piece.repeat(bytes / piece.len())
}

struct Fixture {
    app: App,
    terminal: Terminal<TestBackend>,
    _storage: TempDir,
}

impl Fixture {
    fn empty() -> Self {
        let storage = TempDir::new().unwrap();
        Self {
            app: bench_support::app(StateDir::from_path(storage.path().to_path_buf())),
            terminal: Terminal::new(TestBackend::new(AREA.width, AREA.height)).unwrap(),
            _storage: storage,
        }
    }

    /// Pasted like a user would, so the cursor sits at the end. The assert
    /// is there because a paste some overlay swallows would not crash, it
    /// would just make every bench look fast. The tick and frame pay off what
    /// the paste left behind, so later ticks really are idle.
    fn loaded(doc: &str) -> Self {
        let mut fixture = Self::empty();
        fixture.app.update(Msg::Paste(doc.to_owned()));
        assert!(
            bench_support::input_value(&fixture.app) == doc,
            "the paste never reached the input"
        );
        let _ = fixture.app.tick();
        fixture.frame();
        fixture
    }

    fn frame(&mut self) {
        self.terminal
            .draw(|frame| {
                self.app.view(frame);
            })
            .unwrap();
    }
}

/// Only {measured} is timed. {reset} puts the input back after each pass,
/// or every pass would work on a longer text than the one before.
fn bench_with_reset(
    group: &mut BenchmarkGroup<WallTime>,
    id: BenchmarkId,
    mut fixture: Fixture,
    mut measured: impl FnMut(&mut Fixture),
    mut reset: impl FnMut(&mut Fixture),
) {
    group.bench_function(id, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                measured(&mut fixture);
                total += start.elapsed();
                reset(&mut fixture);
            }
            total
        })
    });
}

fn bench_input(c: &mut Criterion) {
    let mut group = c.benchmark_group("input");
    group.sample_size(SAMPLES);
    let chunk = markdown(PASTE_LINES);
    let typed = TYPED.to_string();

    for &lines in SIZES {
        let doc = markdown(lines);
        let id = |name: &str| BenchmarkId::new(name, lines);

        bench_with_reset(
            &mut group,
            id("paste_empty"),
            Fixture::empty(),
            |f| {
                f.app.update(Msg::Paste(doc.clone()));
            },
            |f| bench_support::set_input(&mut f.app, ""),
        );

        // The cursor parks at the start, so the paste lands above every line
        // and shifts every line start.
        let mut full = Fixture::empty();
        bench_support::set_input(&mut full.app, &doc);
        bench_with_reset(
            &mut group,
            id("paste_nonempty"),
            full,
            |f| {
                f.app.update(Msg::Paste(chunk.clone()));
            },
            |f| bench_support::set_input(&mut f.app, &doc),
        );

        // `paste_nonempty` played backwards. Undo parks the cursor back at the
        // start, so each reset pastes the chunk above every line again.
        let mut pasted = Fixture::empty();
        bench_support::set_input(&mut pasted.app, &doc);
        pasted.app.update(Msg::Paste(chunk.clone()));
        pasted.app.update(Msg::Key(UNDO));
        assert!(
            bench_support::input_value(&pasted.app) == doc,
            "the undo never reached the input"
        );
        pasted.app.update(Msg::Paste(chunk.clone()));
        bench_with_reset(
            &mut group,
            id("undo_paste"),
            pasted,
            |f| {
                f.app.update(Msg::Key(UNDO));
            },
            |f| {
                f.app.update(Msg::Paste(chunk.clone()));
            },
        );

        let mut fixture = Fixture::loaded(&doc);
        group.bench_function(id("frame"), |b| b.iter(|| fixture.frame()));

        bench_with_reset(
            &mut group,
            id("keystroke_frame"),
            Fixture::loaded(&doc),
            |f| {
                f.app.update(Msg::Key(KeyCode::Char(TYPED).into()));
                f.frame();
            },
            |f| {
                f.app.update(Msg::Key(KeyCode::Backspace.into()));
            },
        );

        let end = doc.len();
        bench_with_reset(
            &mut group,
            id("plugin_edit"),
            Fixture::loaded(&doc),
            |f| bench_support::plugin_edit(&mut f.app, AREA, end, end, &typed).unwrap(),
            |f| bench_support::plugin_edit(&mut f.app, AREA, end, end + typed.len(), "").unwrap(),
        );

        let mut fixture = Fixture::loaded(&doc);
        group.bench_function(id("idle_tick"), |b| {
            b.iter(|| black_box(fixture.app.tick()))
        });
    }

    // One enormous line is a different load from many short ones: wrapping
    // splits it into thousands of rows, and the cursor, sitting at the end
    // of it, is a column far from the line start.
    let mut single_line_paste = |label: &str, piece: &str| {
        let line = single_line(SINGLE_LINE_BYTES, piece);
        let mut fixture = Fixture::empty();
        fixture.app.update(Msg::Paste(line.clone()));
        assert!(
            bench_support::input_value(&fixture.app) == line,
            "the paste never reached the input"
        );
        bench_support::set_input(&mut fixture.app, "");
        bench_with_reset(
            &mut group,
            BenchmarkId::new("paste_single_line", label),
            fixture,
            |f| {
                f.app.update(Msg::Paste(line.clone()));
                f.frame();
            },
            |f| bench_support::set_input(&mut f.app, ""),
        );
    };
    single_line_paste("words", WORD);
    single_line_paste("unbroken", UNBROKEN);

    // A keystroke on the same megabyte line, where the cursor is a char
    // column far from the line start: what that costs per keystroke.
    let line = single_line(SINGLE_LINE_BYTES, UNBROKEN);
    for (label, column) in [("end", SINGLE_LINE_BYTES), ("middle", MIDDLE_COLUMN)] {
        let mut fixture = Fixture::loaded(&line);
        bench_support::set_cursor(&mut fixture.app, 0, column);
        fixture.frame();
        bench_with_reset(
            &mut group,
            BenchmarkId::new("keystroke_single_line", label),
            fixture,
            |f| {
                f.app.update(Msg::Key(KeyCode::Char(TYPED).into()));
                f.frame();
            },
            |f| {
                f.app.update(Msg::Key(KeyCode::Backspace.into()));
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_input);
criterion_main!(benches);
