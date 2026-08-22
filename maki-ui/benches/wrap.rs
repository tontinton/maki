use criterion::{Criterion, black_box, criterion_group, criterion_main};
use maki_ui::wrap;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;

const LINE_COUNT: usize = 7800;
const NARROW: u16 = 40;
const WIDE: u16 = 100;

const PROSE: &[&str] = &[
    "the agent loop pulls a message off the channel and hands it to the provider",
    "short",
    "",
    "a resize re-measures every segment in the transcript, which is why this is hot",
    "  indented continuation of the previous thought, with trailing spaces   ",
    "tool output: read file=/home/user/project/src/components/messages/segment.rs",
];

fn corpus() -> Vec<Line<'static>> {
    (0..LINE_COUNT)
        .map(|i| match i % 4 {
            0 => Line::from(vec![
                Span::styled("maki> ", Style::default().cyan()),
                Span::raw(PROSE[i % PROSE.len()].to_string()),
                Span::styled(" [done]", Style::default().green()),
            ]),
            1 => Line::from(vec![
                Span::raw("你好世界 ".to_string()),
                Span::raw(PROSE[(i + 1) % PROSE.len()].to_string()),
            ]),
            _ => Line::from(Span::raw(PROSE[(i + 2) % PROSE.len()].to_string())),
        })
        .collect()
}

fn ratatui_rows(lines: &[Line<'static>], width: u16) -> u16 {
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width) as u16
}

fn bench_wrap(c: &mut Criterion) {
    let lines = corpus();

    c.bench_function("wrap_total_rows_7800", |b| {
        b.iter(|| wrap::total_rows(black_box(&lines), WIDE))
    });
    c.bench_function("ratatui_line_count_7800", |b| {
        b.iter(|| ratatui_rows(black_box(&lines), WIDE))
    });
    c.bench_function("wrap_total_rows_7800_narrow", |b| {
        b.iter(|| wrap::total_rows(black_box(&lines), NARROW))
    });
    c.bench_function("ratatui_line_count_7800_narrow", |b| {
        b.iter(|| ratatui_rows(black_box(&lines), NARROW))
    });
}

criterion_group!(benches, bench_wrap);
criterion_main!(benches);
