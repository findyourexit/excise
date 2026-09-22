use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use excise::animation::AnimationScheduler;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn benchmark_fade(c: &mut Criterion) {
    let surfaces = [
        ("80x24", Rect::new(0, 0, 80, 24)),
        ("160x50", Rect::new(0, 0, 160, 50)),
        ("200x80", Rect::new(0, 0, 200, 80)),
    ];
    let mut group = c.benchmark_group("animation/scheduler-completion");
    for (label, surface) in surfaces {
        let header = Rect::new(0, 0, surface.width, 3);
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &surface,
            |b, &surface| {
                b.iter_batched(
                    || {
                        let mut buffer = Buffer::empty(surface);
                        let mut scheduler = AnimationScheduler::new(false, false, Duration::ZERO);
                        scheduler.schedule_completion();
                        scheduler.process(Duration::ZERO, &mut buffer, header, surface);
                        (scheduler, buffer)
                    },
                    |(mut scheduler, mut buffer)| {
                        let next = scheduler
                            .next_frame_at()
                            .expect("scheduled completion should request a frame");
                        scheduler.process(next, &mut buffer, header, surface);
                        black_box((scheduler.next_frame_at(), buffer[(0, 0)].fg));
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(benches, benchmark_fade);
criterion_main!(benches);
