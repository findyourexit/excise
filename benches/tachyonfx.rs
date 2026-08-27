use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use excise::animation::AnimationScheduler;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn benchmark_fade(c: &mut Criterion) {
    let surface = Rect::new(0, 0, 100, 30);
    let header = Rect::new(0, 0, surface.width, 3);
    c.bench_function("animation/scheduler_completion/100x30", |b| {
        b.iter_batched(
            || {
                let mut buffer = Buffer::empty(surface);
                let mut scheduler = AnimationScheduler::new(false, false, Duration::ZERO);
                scheduler.schedule_completion();
                scheduler.process(Duration::ZERO, &mut buffer, header, surface);
                (scheduler, buffer)
            },
            |(mut scheduler, mut buffer)| {
                scheduler.process(Duration::from_millis(8), &mut buffer, header, surface);
                black_box(buffer[(0, 0)].fg);
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, benchmark_fade);
criterion_main!(benches);
