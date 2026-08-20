use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use tachyonfx::{Duration, fx};

fn benchmark_fade(c: &mut Criterion) {
    let area = Rect::new(0, 0, 100, 30);
    c.bench_function("tachyonfx/fade_to_fg/100x30", |b| {
        b.iter_batched(
            || {
                let mut buffer = Buffer::empty(area);
                buffer[(0, 0)].set_fg(Color::White);
                (fx::fade_to_fg(Color::Black, 200), buffer)
            },
            |(mut effect, mut buffer)| {
                effect.process(Duration::from_millis(8), &mut buffer, area);
                black_box(buffer[(0, 0)].fg);
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, benchmark_fade);
criterion_main!(benches);
