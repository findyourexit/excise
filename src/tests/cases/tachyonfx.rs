use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use tachyonfx::{Duration, fx};

#[test]
fn tachyonfx_processes_the_project_ratatui_buffer() {
    let area = Rect::new(0, 0, 1, 1);
    let mut buffer = Buffer::empty(area);
    buffer[(0, 0)].set_fg(Color::White);

    let mut fade = fx::fade_to_fg(Color::Black, 100);
    fade.process(Duration::from_millis(50), &mut buffer, area);

    assert_ne!(buffer[(0, 0)].fg, Color::White);
    assert_ne!(buffer[(0, 0)].fg, Color::Black);

    fade.process(Duration::from_millis(50), &mut buffer, area);
    assert_eq!(buffer[(0, 0)].fg, Color::Black);
}
