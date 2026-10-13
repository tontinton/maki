//! What a long editing session leaves behind in the input box. Not a timing
//! bench: it replays 100k edits in rounds of typing, pastes, undo, redo and
//! plugin edits with a frame every few of them, and prints the heap in use
//! at checkpoints. Every round puts the text back as it was, so the heap
//! should settle after the first few and stay put. A line that keeps
//! climbing is the undo stack, the change log, the marks or a cache growing
//! without bound.
//!
//! The heap is the process's, from glibc, so the worker threads' trees count
//! too. Run it on its own: `cargo bench --profile dev -p maki-ui --bench
//! input_memory`.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod report {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use maki_storage::StateDir;
    use maki_ui::app::{App, Msg};
    use maki_ui::bench_support::{self, Rules};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use tempfile::TempDir;

    const DOC_LINES: usize = 1_000;
    const MARKS: usize = 10_000;
    const OPS: usize = 100_000;
    const OPS_PER_ROUND: usize = 10;
    const ROUNDS: usize = OPS / OPS_PER_ROUND;
    const CHECKPOINT_EVERY: usize = 10_000;
    const FRAME_EVERY: usize = 3;
    const REDECORATE_EVERY: usize = 1_000;
    const PASTE_LINES: usize = 5;
    const AREA: Rect = Rect::new(0, 0, 120, 40);
    const TYPED: [char; 2] = ['x', 'y'];
    const UNDO: KeyEvent = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT);
    const MIB: f64 = (1 << 20) as f64;
    const PARAGRAPH_BREAK: &str = "\n\n";

    fn line(i: usize) -> String {
        format!("line {i} with **bold** text, `code` and a [link](https://example.com/{i})")
    }

    /// Blank lines between the lines, so markdown reads them as the short
    /// paragraphs a prompt holds rather than one long paragraph.
    fn paragraphs(count: usize) -> String {
        (0..count)
            .map(line)
            .collect::<Vec<_>>()
            .join(PARAGRAPH_BREAK)
    }

    fn heap_in_use() -> usize {
        // SAFETY: `mallinfo2` only reads the allocator's own counters.
        let info = unsafe { libc::mallinfo2() };
        info.uordblks + info.hblkhd
    }

    struct Session {
        app: App,
        terminal: Terminal<TestBackend>,
        _storage: TempDir,
    }

    impl Session {
        fn new(rules: Option<Rules>) -> Self {
            let storage = TempDir::new().unwrap();
            let mut app = bench_support::app(StateDir::from_path(storage.path().to_path_buf()));
            if let Some(rules) = rules {
                bench_support::set_rules(&mut app, rules);
            }
            Self {
                app,
                terminal: Terminal::new(TestBackend::new(AREA.width, AREA.height)).unwrap(),
                _storage: storage,
            }
        }

        fn frame(&mut self) {
            let _ = self.app.tick();
            self.terminal
                .draw(|frame| {
                    self.app.view(frame);
                })
                .unwrap();
            bench_support::settle_parse(&mut self.app);
        }

        fn key(&mut self, key: impl Into<KeyEvent>) {
            self.app.update(Msg::Key(key.into()));
        }

        fn op(&mut self, step: usize, paste: &str) {
            let end = bench_support::input_value(&self.app).len();
            match step {
                0 | 1 => self.key(KeyCode::Char(TYPED[step])),
                2 | 9 => self.key(KeyCode::Backspace),
                3 => drop(self.app.update(Msg::Paste(paste.to_owned()))),
                4 | 6 => self.key(UNDO),
                5 => assert!(bench_support::redo(&mut self.app), "nothing to redo"),
                7 => bench_support::plugin_edit(&mut self.app, AREA, end, end, "x").unwrap(),
                8 => bench_support::plugin_edit(&mut self.app, AREA, end - 1, end, "").unwrap(),
                _ => unreachable!("a round has {OPS_PER_ROUND} steps"),
            }
        }
    }

    fn replay(label: &str, rules: Option<Rules>) {
        let mut session = Session::new(rules);
        let empty = heap_in_use();
        let doc = paragraphs(DOC_LINES);
        let paste = paragraphs(PASTE_LINES);
        session.app.update(Msg::Paste(doc.clone()));
        assert!(
            bench_support::input_value(&session.app) == doc,
            "the paste never reached the input"
        );
        bench_support::decorate(&mut session.app, MARKS);
        session.frame();
        let loaded = heap_in_use();
        let mut peak = loaded;

        println!(
            "{label}: {} bytes of text, {MARKS} marks, empty app {:.2} MiB, loaded {:.2} MiB",
            doc.len(),
            empty as f64 / MIB,
            loaded as f64 / MIB
        );
        for round in 1..=ROUNDS {
            for step in 0..OPS_PER_ROUND {
                session.op(step, &paste);
                if (round * OPS_PER_ROUND + step).is_multiple_of(FRAME_EVERY) {
                    session.frame();
                    peak = peak.max(heap_in_use());
                }
            }
            assert_eq!(
                bench_support::input_value(&session.app).len(),
                doc.len(),
                "round {round} left the text changed"
            );
            if round.is_multiple_of(REDECORATE_EVERY / OPS_PER_ROUND) {
                bench_support::decorate(&mut session.app, MARKS);
            }
            if round.is_multiple_of(CHECKPOINT_EVERY / OPS_PER_ROUND) {
                session.frame();
                let now = heap_in_use();
                println!(
                    "  after {:>6} ops: {:>8.2} MiB ({:+.2} MiB since loaded), text {} bytes",
                    round * OPS_PER_ROUND,
                    now as f64 / MIB,
                    (now as f64 - loaded as f64) / MIB,
                    bench_support::input_value(&session.app).len()
                );
            }
        }

        bench_support::set_input(&mut session.app, "");
        bench_support::decorate(&mut session.app, 0);
        session.frame();
        let cleared = heap_in_use();
        println!(
            "  sampled peak {:.2} MiB; emptied: {:.2} MiB, {:+.2} MiB over the empty app",
            peak as f64 / MIB,
            cleared as f64 / MIB,
            (cleared as f64 - empty as f64) / MIB
        );
    }

    pub fn main() {
        replay("plain", None);
        replay("regex", Some(Rules::Regex));
        replay("syntax", Some(Rules::Syntax));
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn main() {
    report::main();
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn main() {
    println!("the heap is read from glibc, so this only runs on linux-gnu");
}
