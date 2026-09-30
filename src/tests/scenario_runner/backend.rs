//! The terminal the scenario runner drives: a ratatui `TestBackend` that the input source can
//! read while the owner loop draws to it.
//!
//! `runtime::run` takes ownership of its backend, so the runner keeps a second handle to the same
//! buffer. Everything runs on the owner thread, so a `RefCell` is enough: draws happen inside the
//! loop and reads happen inside `InputSource::read`, never at the same time.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;

use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

use super::screen::Screen;

/// A shared `TestBackend`. Cloning shares the same terminal.
#[derive(Clone)]
pub struct SharedBackend {
    terminal: Rc<RefCell<TestBackend>>,
}

impl SharedBackend {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            terminal: Rc::new(RefCell::new(TestBackend::new(cols, rows))),
        }
    }

    /// The text of the terminal as it is now.
    pub fn screen(&self) -> Screen {
        Screen::from_buffer(self.terminal.borrow().buffer())
    }

    /// Changes the terminal size. The owner loop notices at its next draw, after the runner also
    /// delivers a resize event.
    pub fn resize(&self, cols: u16, rows: u16) {
        self.terminal.borrow_mut().resize(cols, rows);
    }
}

impl Backend for SharedBackend {
    type Error = Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.terminal.borrow_mut().draw(content)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.terminal.borrow_mut().hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.terminal.borrow_mut().show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.terminal.borrow_mut().get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.terminal.borrow_mut().set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.terminal.borrow_mut().clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.terminal.borrow_mut().clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.terminal.borrow().size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.terminal.borrow_mut().window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.terminal.borrow_mut().flush()
    }
}
