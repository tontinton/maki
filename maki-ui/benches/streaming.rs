//! A reply grows a few characters per frame and every frame renders it again,
//! so the markdown parser runs once per appended chunk. These benches stream
//! whole messages the way the transcript draws them: the reply in full, and
//! live thinking as its collapsed tail.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use maki_config::DEFAULT_THINKING_LINES;
use maki_ui::bench_support::LiveMessage;
use maki_ui::text_to_lines;
use ratatui::style::Style;

const WIDTH: u16 = 100;
const CHUNK_BYTES: usize = 4;
const SECTIONS: usize = 5;
const LIST_ITEMS: usize = 150;
const NESTED_EVERY: usize = 5;
const QUOTE_LINES: usize = 150;
const SAMPLES: usize = 20;
const PREFIX: &str = "maki> ";

fn section(i: usize) -> String {
    format!(
        "\
## Step {i}: what the reflow does

The agent loop pulls a message off the channel and hands it to the provider, then
waits for the **first token** before it paints anything. Every `render_lines` call
reads the whole buffer again, so a long reply costs more per frame than a short one.
See [the ratatui docs](https://docs.rs/ratatui) for how a `Line` wraps.

1. Measure the transcript once per resize.
2. Keep the highlighter alive across frames:
   - one `CodeHighlighter` per fenced block
   - a fresh one when the *language* changes
     - nested deeper, with `inline code` and **bold**
3. Repaint only what moved.

```rust
fn reflow_segment(&mut self, seg_idx: usize /* {i} */, width: u16) {{
    let Some(seg) = self.cache.get_mut(seg_idx) else {{
        return;
    }};
    seg.stale = false;
    if let Some(tid) = seg.tool_id.clone() {{
        self.rebuild_tool_segment(&tid);
    }} else {{
        self.reflow_text_segment(seg_idx, width);
    }}
}}
```

> The cache key holds the theme generation, so switching themes
> repaints even when the text is unchanged.
> - a quoted list item
> - another one, with `code`

| step | before | after |
|------|-------:|------:|
| parse | 406 µs | 35 µs |
| wrap | 31 µs | 3 µs |

中文说明：**注意：**请先备份配置文件，然后再运行 `maki update`。这一段用来测试宽字符的换行和强调。
日本語の説明も少し入れておきます。*重要*な部分は**太字**で書きます。

```python
def summarize(rows):  # {i}
    return {{r[\"step\"]: r[\"after\"] for r in rows}}
```

---

That is it for step {i}, and the next one builds on it.

"
    )
}

fn message() -> String {
    (0..SECTIONS).map(section).collect()
}

fn long_list() -> String {
    (0..LIST_ITEMS)
        .map(|i| {
            let item = format!(
                "- item {i}: the parser sees `code`, **bold** and plain prose that runs on\n"
            );
            if i % NESTED_EVERY == 0 {
                format!("{item}  - nested detail {i} with *emphasis*\n")
            } else {
                item
            }
        })
        .collect()
}

fn long_quote() -> String {
    (0..QUOTE_LINES)
        .map(|i| format!("> line {i} of a long quote, with *emphasis* and `code` in it\n"))
        .collect()
}

/// Appends `text` a few bytes at a time and draws after each append.
fn stream(text: &str, mut live: LiveMessage, draw: fn(&mut LiveMessage) -> usize) -> usize {
    let mut drawn = 0;
    let mut sent = 0;
    while sent < text.len() {
        let mut end = (sent + CHUNK_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end += 1;
        }
        live.push(&text[sent..end]);
        drawn += draw(&mut live);
        sent = end;
    }
    drawn
}

fn draw_reply(live: &mut LiveMessage) -> usize {
    live.render(WIDTH)
}

fn draw_thinking(live: &mut LiveMessage) -> usize {
    live.render_tail(DEFAULT_THINKING_LINES, WIDTH)
}

fn bench_streaming(c: &mut Criterion) {
    let message = message();
    let list = long_list();
    let quote = long_quote();
    println!(
        "message {} bytes, list {} bytes, quote {} bytes",
        message.len(),
        list.len(),
        quote.len()
    );

    let mut g = c.benchmark_group("streaming");
    g.sample_size(SAMPLES);
    for (name, text) in [
        ("message", &message),
        ("long_list", &list),
        ("long_quote", &quote),
    ] {
        g.bench_function(format!("text_{name}"), |b| {
            b.iter(|| stream(black_box(text), LiveMessage::text(), draw_reply))
        });
        g.bench_function(format!("thinking_{name}"), |b| {
            b.iter(|| stream(black_box(text), LiveMessage::thinking(), draw_thinking))
        });
    }
    g.finish();

    let mut g = c.benchmark_group("full");
    g.bench_function("render_message", |b| {
        b.iter(|| {
            text_to_lines(
                black_box(&message),
                PREFIX,
                Style::default(),
                Style::default(),
                WIDTH,
                None,
            )
            .len()
        })
    });
    g.bench_function("parse_message", |b| {
        b.iter(|| maki_markdown::parse(black_box(&message)).len())
    });
    g.finish();
}

criterion_group!(benches, bench_streaming);
criterion_main!(benches);
