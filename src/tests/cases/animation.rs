use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::animation::AnimationScheduler;

#[test]
fn keyed_effects_coalesce_and_finish_under_virtual_time() {
    let area = Rect::new(0, 0, 20, 4);
    let mut buffer = Buffer::empty(area);
    let mut scheduler = AnimationScheduler::new(false, false, Duration::ZERO);

    for _ in 0..1_000 {
        scheduler.schedule_navigation();
    }
    assert_eq!(scheduler.pending_slots(), 1);

    scheduler.process(Duration::ZERO, &mut buffer, area);
    assert!(scheduler.is_running());
    scheduler.process(Duration::from_millis(500), &mut buffer, area);
    assert!(!scheduler.is_running());
}

#[test]
fn reduced_motion_and_monochrome_are_idle_silent() {
    let mut reduced = AnimationScheduler::new(true, false, Duration::ZERO);
    reduced.schedule_navigation();
    reduced.schedule_state_change();
    assert!(!reduced.is_running());

    let mut monochrome = AnimationScheduler::new(false, true, Duration::ZERO);
    monochrome.schedule_navigation();
    assert!(!monochrome.is_running());
}

#[test]
fn cancellation_removes_effect_before_modal_rendering() {
    let area = Rect::new(0, 0, 20, 4);
    let mut buffer = Buffer::empty(area);
    let mut scheduler = AnimationScheduler::new(false, false, Duration::ZERO);
    scheduler.schedule_navigation();
    scheduler.process(Duration::ZERO, &mut buffer, area);
    scheduler.cancel_all();
    scheduler.process(Duration::from_millis(1), &mut buffer, area);
    assert!(!scheduler.is_running());
}
