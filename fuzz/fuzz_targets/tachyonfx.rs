#![no_main]

use std::time::Duration;

use excise::animation::AnimationScheduler;
use libfuzzer_sys::fuzz_target;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }

    let area = Rect::new(
        0,
        0,
        u16::from(data[0] % 80 + 1),
        u16::from(data[1] % 40 + 1),
    );
    let mut buffer = Buffer::empty(area);
    let mut scheduler = AnimationScheduler::new(data[2] & 1 != 0, data[2] & 2 != 0, Duration::ZERO);
    for byte in &data[3..] {
        if byte & 1 == 0 {
            scheduler.schedule_navigation();
        } else {
            scheduler.schedule_state_change();
        }
        if byte & 2 != 0 {
            scheduler.cancel_all();
        }
        scheduler.process(Duration::from_millis(u64::from(*byte)), &mut buffer, area);
        assert!(scheduler.pending_slots() <= 2);
    }
});
