use criterion::{Criterion, black_box, criterion_group, criterion_main};
use maki_markdown::render::Renderer;
use maki_ui::text_to_lines;
use maki_ui::wrap;
use ratatui::style::Style;
use ratatui::text::Line;
use syntect::highlighting::{Highlighter as SynHighlighter, Theme as SynTheme, ThemeSet};
use syntect::parsing::ParseState;

const MESSAGE_COUNT: usize = 600;
const KIND_COUNT: usize = 120;
const WIDE: u16 = 100;
const NARROW: u16 = 80;

const PROSE: &str = "\
The agent loop pulls a message off the channel and hands it to the provider, then
waits for the first token before it paints anything at all.

- a resize re-measures every segment in the transcript
- `text_to_lines` builds a fresh renderer per call
- the highlight worker is width independent by design

That is the whole trick, and it only works because nothing downstream of the
parse cares about how wide the terminal happens to be right now.
";

const CODE_BLOCK: &str = "\
fn reflow_segment(&mut self, seg_idx: usize, width: u16) {
    let Some(seg) = self.cache.get_mut(seg_idx) else {
        return;
    };
    seg.stale = false;
    let (tool_id, msg_idx) = (seg.tool_id.clone(), seg.msg_index);
    if let Some(tid) = tool_id {
        self.rebuild_tool_segment(&tid);
    } else if msg_idx.is_some() {
        self.reflow_text_segment(seg_idx, width);
    }
}
";

/// The one substring `message` rewrites to keep every code block distinct.
const CODE_ANCHOR: &str = "seg_idx: usize";

const WITH_TABLE: &str = "\
Measured, 600 messages, ten successive resizes:

| what | debug | release |
|---|---|---|
| ratatui | 406 ms | 35.2 ms |
| wrap::total_rows | 31 ms | 3.3 ms |

Wrapping is no longer the lever it was.
";

/// The fence has to sit at column zero. Indented four spaces or more, the
/// parser reads it as prose and the corpus stops exercising the highlighter,
/// which is the one thing these benchmarks are here to measure.
fn with_code() -> String {
    format!(
        "Here is the hot path, cleaned up a little:\n\n```rust\n{CODE_BLOCK}```\n\nThe width only reaches the code block, everything above it is width free.\n"
    )
}

/// Every code block is distinct: a transcript where 120 messages shared one
/// block would flatter any content cache.
fn message(body: &str, i: usize) -> String {
    let body = body.replace(CODE_ANCHOR, &format!("{CODE_ANCHOR} /* {i} */"));
    format!("{body}\nmessage {i}\n")
}

fn transcript() -> Vec<String> {
    let with_code = with_code();
    (0..MESSAGE_COUNT)
        .map(|i| match i % 5 {
            0 => message(&with_code, i),
            3 => message(WITH_TABLE, i),
            _ => message(PROSE, i),
        })
        .collect()
}

fn rebuild(texts: &[String], width: u16) -> usize {
    texts
        .iter()
        .map(|t| text_to_lines(t, "maki> ", Style::default(), Style::default(), width, None).len())
        .sum()
}

fn parse_only(texts: &[String], width: u16) -> usize {
    texts
        .iter()
        .map(|t| Renderer::unwrapped().render(t, width).len())
        .sum()
}

fn blocks_only(texts: &[String]) -> usize {
    texts.iter().map(|t| maki_markdown::parse(t).len()).sum()
}

fn kind_corpus(body: &str) -> Vec<String> {
    (0..KIND_COUNT).map(|i| message(body, i)).collect()
}

fn bench_highlight(c: &mut Criterion) {
    let real_theme = ThemeSet::load_defaults().themes["base16-ocean.dark"].clone();
    let mut g = c.benchmark_group("highlight_one_block");
    g.sample_size(20);

    g.bench_function("syn_highlighter_new_real_theme", |b| {
        b.iter(|| SynHighlighter::new(black_box(&real_theme)))
    });
    g.bench_function("parse_lines_only", |b| {
        b.iter(|| {
            let mut ps = ParseState::new(maki_highlight::syntax_for_token("rust"));
            for line in CODE_BLOCK.lines() {
                let _ = ps.parse_line(line, maki_highlight::syntax_set());
            }
        })
    });
    for (name, theme) in [
        ("empty_theme", SynTheme::default()),
        ("real_theme", real_theme.clone()),
    ] {
        maki_highlight::set_theme(theme);
        g.bench_function(format!("code_highlighter_update_{name}"), |b| {
            b.iter(|| {
                maki_highlight::CodeHighlighter::new("rust")
                    .update(black_box(CODE_BLOCK))
                    .len()
            })
        });
    }
    g.finish();
}

fn bench_reflow(c: &mut Criterion) {
    let texts = transcript();
    let lines: Vec<Line<'static>> = texts
        .iter()
        .flat_map(|t| text_to_lines(t, "maki> ", Style::default(), Style::default(), WIDE, None))
        .collect();
    println!("corpus: {MESSAGE_COUNT} messages, {} lines", lines.len());

    c.bench_function("rebuild_600_messages", |b| {
        b.iter(|| rebuild(black_box(&texts), NARROW))
    });
    c.bench_function("parse_only_600_messages", |b| {
        b.iter(|| parse_only(black_box(&texts), NARROW))
    });
    c.bench_function("measure_600_messages", |b| {
        b.iter(|| wrap::total_rows(black_box(&lines), NARROW))
    });

    let mut kinds = c.benchmark_group("by_kind_120");
    kinds.sample_size(20);
    let with_code = with_code();
    for (name, body) in [
        ("prose", PROSE),
        ("code", with_code.as_str()),
        ("table", WITH_TABLE),
    ] {
        let corpus = kind_corpus(body);
        kinds.bench_function(format!("render_{name}"), |b| {
            b.iter(|| parse_only(black_box(&corpus), NARROW))
        });
        kinds.bench_function(format!("blocks_{name}"), |b| {
            b.iter(|| blocks_only(black_box(&corpus)))
        });
    }
    kinds.finish();
}

criterion_group!(benches, bench_reflow, bench_highlight);
criterion_main!(benches);
