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
        scheduler.schedule_completion();
        scheduler.schedule_error();
        scheduler.schedule_deletion_result();
    }
    assert_eq!(scheduler.pending_slots(), 3);

    scheduler.process(Duration::ZERO, &mut buffer, area, area);
    assert!(scheduler.is_running());
    scheduler.process(Duration::from_millis(500), &mut buffer, area, area);
    assert!(!scheduler.is_running());
}

#[test]
fn a_fresh_acknowledgement_starts_after_the_previous_frame_elapsed() {
    let area = Rect::new(0, 0, 20, 4);
    let mut buffer = Buffer::empty(area);
    let mut scheduler = AnimationScheduler::new(false, false, Duration::ZERO);

    scheduler.schedule_completion();
    scheduler.process(Duration::ZERO, &mut buffer, area, area);

    scheduler.schedule_error();
    scheduler.process(Duration::from_millis(200), &mut buffer, area, area);
    scheduler.process(Duration::from_millis(240), &mut buffer, area, area);
    assert!(scheduler.is_running());

    scheduler.process(Duration::from_millis(400), &mut buffer, area, area);
    assert!(!scheduler.is_running());
}

#[test]
fn delayed_acknowledgements_start_at_zero_without_reprocessing_active_keys() {
    let area = Rect::new(0, 0, 20, 4);
    let mut buffer = Buffer::empty(area);
    let mut scheduler = AnimationScheduler::new(false, false, Duration::ZERO);
    let delayed = Duration::from_secs(5);

    scheduler.schedule_completion();
    scheduler.process(Duration::ZERO, &mut buffer, area, area);

    scheduler.schedule_error();
    scheduler.process(delayed, &mut buffer, area, area);
    assert!(scheduler.is_running());

    scheduler.process(
        delayed + Duration::from_millis(199),
        &mut buffer,
        area,
        area,
    );
    assert!(scheduler.is_running());
    scheduler.process(
        delayed + Duration::from_millis(200),
        &mut buffer,
        area,
        area,
    );
    assert!(!scheduler.is_running());
}

#[test]
fn replacing_an_active_acknowledgement_restarts_only_that_key() {
    let area = Rect::new(0, 0, 20, 4);
    let mut buffer = Buffer::empty(area);
    let mut scheduler = AnimationScheduler::new(false, false, Duration::ZERO);

    scheduler.schedule_completion();
    scheduler.process(Duration::ZERO, &mut buffer, area, area);
    scheduler.schedule_error();
    scheduler.process(Duration::from_millis(1), &mut buffer, area, area);
    scheduler.process(Duration::from_millis(100), &mut buffer, area, area);

    // Replacing the error must not restart or discard the completion.
    scheduler.schedule_error();
    scheduler.process(Duration::from_millis(100), &mut buffer, area, area);
    scheduler.process(Duration::from_millis(250), &mut buffer, area, area);
    assert!(
        scheduler.is_running(),
        "the replacement error must outlive both original effects"
    );
    scheduler.process(Duration::from_millis(300), &mut buffer, area, area);
    assert!(
        !scheduler.is_running(),
        "replacing one key must not restart the completed completion effect"
    );
}

#[test]
fn large_surfaces_use_a_lower_animation_frame_rate() {
    let surface = Rect::new(0, 0, 200, 100);
    let area = Rect::new(0, 0, 20, 4);
    let mut buffer = Buffer::empty(surface);
    let mut scheduler = AnimationScheduler::new(false, false, Duration::ZERO);
    scheduler.schedule_completion();

    scheduler.process(Duration::ZERO, &mut buffer, area, surface);

    assert_eq!(scheduler.next_frame_at(), Some(Duration::from_millis(66)));
}

#[test]
fn reduced_motion_and_monochrome_are_idle_silent() {
    let mut reduced = AnimationScheduler::new(true, false, Duration::ZERO);
    reduced.schedule_completion();
    reduced.schedule_error();
    assert!(!reduced.is_running());

    let mut monochrome = AnimationScheduler::new(false, true, Duration::ZERO);
    monochrome.schedule_completion();
    assert!(!monochrome.is_running());
}

#[test]
fn focus_activity_keeps_frames_until_focus_is_cleared() {
    let area = Rect::new(0, 0, 20, 4);
    let mut buffer = Buffer::empty(area);
    let mut scheduler = AnimationScheduler::new(false, false, Duration::ZERO);
    scheduler.set_activity(true);
    assert!(scheduler.is_running());
    assert_eq!(
        scheduler.next_frame_at(),
        Some(crate::animation::ACTIVE_FRAME_INTERVAL)
    );

    scheduler.process(Duration::from_millis(33), &mut buffer, area, area);
    assert!(scheduler.is_running());
    assert_eq!(scheduler.next_frame_at(), Some(Duration::from_millis(66)));
    scheduler.set_activity(false);
    scheduler.process(Duration::from_millis(66), &mut buffer, area, area);
    assert!(!scheduler.is_running());

    let mut reduced = AnimationScheduler::new(true, false, Duration::ZERO);
    reduced.set_activity(true);
    assert!(!reduced.is_running());
}

#[test]
fn suspended_activity_drains_finite_effects_and_resumes_normally() {
    let area = Rect::new(0, 0, 20, 4);
    let mut buffer = Buffer::empty(area);
    let mut scheduler = AnimationScheduler::new(false, false, Duration::ZERO);

    scheduler.schedule_completion();
    scheduler.process(Duration::ZERO, &mut buffer, area, area);
    scheduler.set_activity(true);
    scheduler.set_activity_suspended(true);
    scheduler.set_activity(true);

    scheduler.process(Duration::from_millis(239), &mut buffer, area, area);
    assert!(scheduler.is_running());
    scheduler.process(Duration::from_millis(240), &mut buffer, area, area);
    assert!(!scheduler.is_running());

    scheduler.set_activity_suspended(false);
    assert!(scheduler.is_running());
    scheduler.set_activity(false);
    assert!(!scheduler.is_running());
}

#[test]
fn cancellation_removes_effect_before_modal_rendering() {
    let area = Rect::new(0, 0, 20, 4);
    let mut buffer = Buffer::empty(area);
    let mut scheduler = AnimationScheduler::new(false, false, Duration::ZERO);
    scheduler.schedule_completion();
    scheduler.process(Duration::ZERO, &mut buffer, area, area);
    scheduler.cancel_all();
    scheduler.process(Duration::from_millis(1), &mut buffer, area, area);
    assert!(!scheduler.is_running());
}

#[test]
fn an_idle_session_does_not_consume_a_freshly_scheduled_effect() {
    let area = Rect::new(0, 0, 20, 4);
    let mut buffer = Buffer::empty(area);
    let mut scheduler = AnimationScheduler::new(false, false, Duration::ZERO);

    // Nothing drew for five seconds, then a keypress schedules an effect. That
    // wait belongs to the idle session, not to the effect.
    let idle = Duration::from_secs(5);
    scheduler.schedule_completion();
    scheduler.process(idle, &mut buffer, area, area);

    assert!(scheduler.is_running());
    assert_eq!(
        scheduler.next_frame_at(),
        Some(idle.saturating_add(crate::animation::ACTIVE_FRAME_INTERVAL))
    );

    scheduler.process(idle, &mut buffer, area, area);
    assert!(scheduler.is_running());

    scheduler.process(idle + Duration::from_millis(239), &mut buffer, area, area);
    assert!(scheduler.is_running());
    scheduler.process(idle + Duration::from_millis(240), &mut buffer, area, area);
    assert!(!scheduler.is_running());

    let mut just_past_buffer = Buffer::empty(area);
    let mut just_past = AnimationScheduler::new(false, false, Duration::ZERO);
    just_past.schedule_completion();
    just_past.process(idle, &mut just_past_buffer, area, area);
    just_past.process(
        idle + Duration::from_millis(241),
        &mut just_past_buffer,
        area,
        area,
    );
    assert!(!just_past.is_running());

    let mut overflowing_buffer = Buffer::empty(area);
    let mut overflowing = AnimationScheduler::new(false, false, Duration::ZERO);
    overflowing.schedule_completion();
    overflowing.process(Duration::ZERO, &mut overflowing_buffer, area, area);
    assert!(overflowing.is_running());
    let beyond_u32_milliseconds = Duration::from_millis(u64::from(u32::MAX) + 1);
    overflowing.process(beyond_u32_milliseconds, &mut overflowing_buffer, area, area);
    assert!(!overflowing.is_running());
}

#[test]
fn a_map_tween_holds_the_fast_cadence_on_a_large_surface() {
    let surface = Rect::new(0, 0, 200, 100);
    let area = Rect::new(0, 0, 20, 4);
    let mut buffer = Buffer::empty(surface);
    let mut scheduler = AnimationScheduler::new(false, false, Duration::ZERO);
    scheduler.process(Duration::ZERO, &mut buffer, area, surface);
    assert_eq!(scheduler.next_frame_at(), None);

    scheduler.set_geometry_active(true);

    let first_frame = crate::animation::ACTIVE_FRAME_INTERVAL;
    let second_frame = first_frame.saturating_add(crate::animation::ACTIVE_FRAME_INTERVAL);
    let third_frame = second_frame.saturating_add(crate::animation::ACTIVE_FRAME_INTERVAL);
    assert!(scheduler.is_running());
    assert_eq!(
        scheduler.next_frame_at(),
        Some(crate::animation::ACTIVE_FRAME_INTERVAL)
    );

    scheduler.process(first_frame, &mut buffer, area, surface);
    assert_eq!(scheduler.next_frame_at(), Some(second_frame));
    scheduler.process(second_frame, &mut buffer, area, surface);
    assert_eq!(scheduler.next_frame_at(), Some(third_frame));

    scheduler.set_geometry_active(false);
    assert_eq!(scheduler.next_frame_at(), None);
}

#[test]
fn a_two_colour_session_still_moves_its_map_but_reduced_motion_does_not() {
    let area = Rect::new(0, 0, 20, 4);
    let mut buffer = Buffer::empty(area);

    let mut monochrome = AnimationScheduler::new(false, true, Duration::ZERO);
    monochrome.set_geometry_active(true);
    assert!(monochrome.is_running());

    monochrome.process(
        crate::animation::ACTIVE_FRAME_INTERVAL,
        &mut buffer,
        area,
        area,
    );
    assert_eq!(monochrome.next_frame_at(), Some(Duration::from_millis(66)));

    let mut reduced = AnimationScheduler::new(false, false, Duration::ZERO);
    reduced.set_geometry_active(true);
    reduced.set_accessibility(true, false);
    reduced.process(Duration::from_millis(1), &mut buffer, area, area);
    assert!(!reduced.is_running());
}
