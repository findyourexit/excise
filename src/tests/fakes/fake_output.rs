use ::std::collections::{HashMap, VecDeque};
use ::std::io;
use ::std::sync::{Arc, Mutex};
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

#[derive(Hash, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    Clear,
    HideCursor,
    ShowCursor,
    GetCursor,
    Flush,
    Draw,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendOperation {
    Clear,
    Draw,
    HideCursor,
    ShowCursor,
    Size,
    Flush,
}

pub struct TestBackend {
    pub events: Arc<Mutex<Vec<TerminalEvent>>>,
    pub draw_events: Arc<Mutex<Vec<String>>>,
    terminal_width: Arc<Mutex<u16>>,
    terminal_height: Arc<Mutex<u16>>,
    initial_frame_seen: bool,
    screen: HashMap<Point, String>,
    failures: Arc<Mutex<VecDeque<BackendOperation>>>,
}

impl TestBackend {
    pub fn new(
        log: Arc<Mutex<Vec<TerminalEvent>>>,
        draw_events: Arc<Mutex<Vec<String>>>,
        terminal_width: Arc<Mutex<u16>>,
        terminal_height: Arc<Mutex<u16>>,
    ) -> Self {
        Self {
            events: log,
            draw_events,
            terminal_width,
            terminal_height,
            initial_frame_seen: false,
            screen: HashMap::new(),
            failures: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn failure_handle(&self) -> Arc<Mutex<VecDeque<BackendOperation>>> {
        self.failures.clone()
    }

    fn fail_if_requested(&self, operation: BackendOperation) -> io::Result<()> {
        let mut failures = self.failures.lock().expect("Failed to lock mutex");
        if failures.front() == Some(&operation) {
            failures.pop_front();
            Err(io::Error::other(format!("injected {operation:?} failure")))
        } else {
            Ok(())
        }
    }
}

#[derive(Hash, Eq, PartialEq)]
struct Point {
    x: u16,
    y: u16,
}

impl Backend for TestBackend {
    type Error = io::Error;

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        if clear_type == ClearType::All {
            self.clear()
        } else {
            Ok(())
        }
    }

    fn clear(&mut self) -> io::Result<()> {
        self.fail_if_requested(BackendOperation::Clear)?;
        self.events
            .lock()
            .expect("Failed to lock mutex")
            .push(TerminalEvent::Clear);
        self.screen.clear();
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.fail_if_requested(BackendOperation::HideCursor)?;
        self.events
            .lock()
            .expect("Failed to lock mutex")
            .push(TerminalEvent::HideCursor);
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.fail_if_requested(BackendOperation::ShowCursor)?;
        self.events
            .lock()
            .expect("Failed to lock mutex")
            .push(TerminalEvent::ShowCursor);
        Ok(())
    }

    fn get_cursor(&mut self) -> io::Result<(u16, u16)> {
        self.events
            .lock()
            .expect("Failed to lock mutex")
            .push(TerminalEvent::GetCursor);
        Ok((0, 0))
    }

    fn set_cursor(&mut self, _x: u16, _y: u16) -> io::Result<()> {
        Ok(())
    }

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.fail_if_requested(BackendOperation::Draw)?;
        self.events
            .lock()
            .expect("Failed to lock mutex")
            .push(TerminalEvent::Draw);
        let (minimum_cells, _) = content.size_hint();
        let mut string = String::with_capacity(minimum_cells * 3);
        for (x, y, cell) in content {
            self.screen.insert(Point { x, y }, cell.symbol().to_owned());
        }
        let terminal_height = self.terminal_height.lock().expect("Failed to lock mutex");
        let terminal_width = self.terminal_width.lock().expect("Failed to lock mutex");
        for y in 0..*terminal_height {
            for x in 0..*terminal_width {
                match self.screen.get(&Point { x, y }) {
                    Some(symbol) => string.push_str(symbol),
                    None => string.push(' '),
                }
            }
            string.push('\n');
        }
        if self.initial_frame_seen {
            self.draw_events
                .lock()
                .expect("Failed to lock mutex")
                .push(string);
        } else {
            self.initial_frame_seen = true;
        }
        Ok(())
    }

    fn size(&self) -> io::Result<Size> {
        self.fail_if_requested(BackendOperation::Size)?;
        let terminal_height = self.terminal_height.lock().expect("Failed to lock mutex");
        let terminal_width = self.terminal_width.lock().expect("Failed to lock mutex");
        Ok(Size::new(*terminal_width, *terminal_height))
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(Position::new(0, 0))
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, _position: P) -> io::Result<()> {
        Ok(())
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        let terminal_height = self.terminal_height.lock().expect("Failed to lock mutex");
        let terminal_width = self.terminal_width.lock().expect("Failed to lock mutex");
        Ok(WindowSize {
            columns_rows: Size::new(*terminal_width, *terminal_height),
            pixels: Size::new(0, 0),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.fail_if_requested(BackendOperation::Flush)?;
        self.events
            .lock()
            .expect("Failed to lock mutex")
            .push(TerminalEvent::Flush);
        Ok(())
    }
}
