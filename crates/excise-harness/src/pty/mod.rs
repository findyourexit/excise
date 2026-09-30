//! Pseudo-terminal sessions.
//!
//! * [`PtySession`] runs a program in a pseudo-terminal of its own process group, feeds its output
//!   to a screen model, records the session, and never leaks the process.
//! * [`Screen`] is the screen model. [`ui`] reads the parts of `excise`'s interface a scenario
//!   asserts on.
//! * [`keys`] encodes key presses and [`cast`] records the session in the asciicast format.

pub mod cast;
pub mod keys;
mod screen;
mod session;
pub mod ui;

pub use screen::{BoxRect, BoxView, HEADER_ROWS, Screen, ScreenModes};
pub use session::{ExitInfo, PtyError, PtySession, SpawnSpec, TerminalModes};
