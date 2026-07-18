use criterion::{Criterion, black_box, criterion_group, criterion_main};
use maki_ui::splash::Splash;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

const ACCENT: Color = Color::Cyan;

fn bench_splash_render(c: &mut Criterion) {
    let splash = Splash::new(true);
    let area = Rect::new(0, 0, 120, 40);
    let mut buf = Buffer::empty(area);

    c.bench_function("splash_render_120x40", |b| {
        b.iter(|| {
            buf.reset();
            splash.render(black_box(area), &mut buf, ACCENT);
        })
    });

    let large_area = Rect::new(0, 0, 200, 60);
    let mut large_buf = Buffer::empty(large_area);

    c.bench_function("splash_render_200x60", |b| {
        b.iter(|| {
            large_buf.reset();
            splash.render(black_box(large_area), &mut large_buf, ACCENT);
        })
    });
}

criterion_group!(benches, bench_splash_render);
criterion_main!(benches);
