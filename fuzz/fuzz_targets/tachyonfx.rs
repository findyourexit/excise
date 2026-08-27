#![no_main]

use std::time::Duration;

use excise::animation::{ACTIVE_FRAME_INTERVAL, AnimationScheduler};
use libfuzzer_sys::fuzz_target;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

const MEDIUM_FRAME_INTERVAL: Duration = Duration::from_millis(50);
const LARGE_FRAME_INTERVAL: Duration = Duration::from_millis(66);
const SMALL_SURFACE_CELLS: u32 = 4_000;
const MEDIUM_SURFACE_CELLS: u32 = 12_000;

fn expected_interval(surface: Rect, geometry_active: bool) -> Duration {
    let cells = u32::from(surface.width) * u32::from(surface.height);
    let baseline = match cells {
        0..=SMALL_SURFACE_CELLS => ACTIVE_FRAME_INTERVAL,
        cells if cells <= MEDIUM_SURFACE_CELLS => MEDIUM_FRAME_INTERVAL,
        _ => LARGE_FRAME_INTERVAL,
    };
    if geometry_active {
        baseline.min(ACTIVE_FRAME_INTERVAL)
    } else {
        baseline
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 5 {
        return;
    }

    let area = Rect::new(
        0,
        0,
        u16::from(data[0] % 80 + 1),
        u16::from(data[1] % 40 + 1),
    );
    // The paint target stays small while the full surface independently crosses
    // every cadence tier, so an accidental area-based cadence is observable.
    let surface = Rect::new(
        0,
        0,
        u16::from(data[3]).saturating_add(1).max(area.width),
        u16::from(data[4]).saturating_add(1).max(area.height),
    );
    let mut buffer = Buffer::empty(surface);
    let mut scheduler = AnimationScheduler::new(data[2] & 1 != 0, data[2] & 2 != 0, Duration::ZERO);
    let mut now = Duration::ZERO;
    for byte in &data[5..] {
        match byte % 5 {
            0 => scheduler.schedule_completion(),
            1 => scheduler.schedule_error(),
            2 => scheduler.schedule_deletion_result(),
            3 => scheduler.cancel_all(),
            _ => scheduler.set_accessibility(byte & 8 != 0, byte & 16 != 0),
        }
        scheduler.set_activity(byte & 32 != 0);
        let geometry_active = byte & 64 != 0;
        scheduler.set_geometry_active(geometry_active);
        now = now.saturating_add(Duration::from_millis(u64::from(*byte)));
        scheduler.process(now, &mut buffer, area, surface);

        let next_frame = scheduler.next_frame_at();
        assert_eq!(next_frame.is_some(), scheduler.is_running());
        if let Some(next_frame) = next_frame {
            assert_eq!(
                next_frame,
                now.saturating_add(expected_interval(surface, geometry_active))
            );
        }
    }

    // Clear whatever persistent state the input left behind, then exercise both
    // finite completion and explicit clearing from known lifecycle states.
    scheduler.set_activity(false);
    scheduler.set_geometry_active(false);
    scheduler.cancel_all();
    scheduler.process(now, &mut buffer, area, surface);
    assert!(!scheduler.is_running());

    scheduler.set_accessibility(false, false);
    scheduler.schedule_completion();
    scheduler.process(now, &mut buffer, area, surface);
    assert!(scheduler.is_running());
    let completion_at = now.saturating_add(Duration::from_millis(240));
    scheduler.process(completion_at, &mut buffer, area, surface);
    assert!(!scheduler.is_running());

    scheduler.schedule_error();
    scheduler.process(completion_at, &mut buffer, area, surface);
    assert!(scheduler.is_running());
    scheduler.cancel_all();
    scheduler.process(completion_at, &mut buffer, area, surface);
    assert!(!scheduler.is_running());
});
