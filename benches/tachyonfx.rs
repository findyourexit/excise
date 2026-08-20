use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use excise::animation::AnimationScheduler;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn benchmark_fade(c: &mut Criterion) {
    let area = Rect::new(0, 0, 100, 30);
    c.bench_function("animation/scheduler_navigation/100x30", |b| {
        b.iter_batched(
            || {
                let buffer = Buffer::empty(area);
                let mut scheduler = AnimationScheduler::new(false, false, Duration::ZERO);
                scheduler.schedule_navigation();
                (scheduler, buffer)
            },
            |(mut scheduler, mut buffer)| {
                scheduler.process(Duration::from_millis(8), &mut buffer, area);
                black_box(buffer[(0, 0)].fg);
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, benchmark_fade);
criterion_main!(benches);
