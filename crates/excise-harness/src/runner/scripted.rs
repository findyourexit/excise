//! Scripted programs for the tests of the protocols in [`super::live`] and of the drivers that
//! share them.
//!
//! A program here is a `/bin/sh` script that writes the event channel and the terminal output the
//! way `excise` does: a `hello`, a `frame` event for every frame it draws, and, when it says so in
//! its `hello`, a mark after the bytes of the frame (see [`crate::pty`]) and an answer to input
//! barrier requests (see "The input barrier" in [`super::live`]). The script decides when each of
//! them is written, which is what lets a test put the screen behind the events by as much as it
//! likes, and wait for the test to say go (a byte on its input) where a step in time would make
//! the test depend on the machine's speed.
//!
//! The scripts run on Unix only.

use std::fmt::Write as _;

/// What every script starts with: the `hello` event, and the functions a script draws with.
///
/// * `report N` writes the event of a frame that has consumed `N` inputs, and numbers it. A later
///   frame that answers a barrier request repeats `N`.
/// * `mark` writes the mark of the latest frame reported, or of the frame given as its argument.
/// * `frame N` is `report N` and `mark`: a frame whose bytes arrive with its event.
/// * `request` counts one barrier request read, and appends a `.` to the file `requests` in the
///   working directory, which a test counts to tell how many requests the program has read.
/// * `answer` is `request` and a frame that repeats the count of inputs of the latest, which
///   `excise` draws for a request: the frame says how many requests have been read.
/// * `key` reads one byte of input, and leaves it in `$byte` as the octal text `od` prints, so
///   that a script can tell Backspace (`177`) from a control byte without a locale. A barrier
///   request is the byte `035`: while `answer_barriers` is 1, which it is for a program that says
///   `input_barrier`, `key` answers each one itself, as `excise` does, and reads on, so it returns
///   only for a byte that is not a request. A script that answers at a moment of its own sets
///   `answer_barriers=0`, sees the byte, and calls `request` and `report` itself.
/// * `serve` reads one byte, which must be a barrier request (the script ends with a status of 9
///   otherwise), and answers it: a script that answers one request, and then goes on with
///   something else, calls it where `key` would read on.
///
/// With `frame_marks` false the `hello` does not say that the program marks its frames, as the
/// `hello` of a program from before the marks does not, and `mark` writes nothing. [`prelude`]
/// gives a program that answers barrier requests exactly when it marks its frames, as `excise`
/// does; [`prelude_with`] says each apart, for a program from between the two.
#[must_use]
pub(crate) fn prelude(frame_marks: bool) -> String {
    prelude_with(frame_marks, frame_marks)
}

/// [`prelude`] for a program that marks its frames, and answers barrier requests, as given.
#[must_use]
pub(crate) fn prelude_with(frame_marks: bool, input_barrier: bool) -> String {
    let marks_field = if frame_marks {
        r#""frame_marks":true,"#
    } else {
        ""
    };
    let barrier_field = if input_barrier {
        r#""input_barrier":true,"#
    } else {
        ""
    };
    let mark = if frame_marks {
        r#"printf '\033]9471;excise-frame=%s\007' "${1:-$seq}""#
    } else {
        ":"
    };
    // A program that answers barrier requests writes how many it has read in every frame.
    let (frame_barriers, frame_barrier_arguments) = if input_barrier {
        (r#""barriers":%s,"#, r#" "$barriers""#)
    } else {
        ("", "")
    };
    let answers = u8::from(input_barrier);
    format!(
        r#"
events="$EXCISE_TEST_EVENTS"
seq=0
inputs=0
barriers=0
answer_barriers={answers}
printf '{{"v":1,"kind":"hello","version":"test","pid":%s,{marks_field}{barrier_field}"t_us":0}}\n' "$$" > "$events"
report() {{
  seq=$((seq + 1))
  inputs="$1"
  printf '{{"v":1,"kind":"frame","seq":%s,"inputs":%s,{frame_barriers}"t_us":1}}\n' "$seq" "$1"{frame_barrier_arguments} >> "$events"
}}
mark() {{
  {mark}
}}
frame() {{
  report "$1"
  mark
}}
request() {{
  barriers=$((barriers + 1))
  printf . >> requests
}}
answer() {{
  request
  frame "$inputs"
}}
key() {{
  while :; do
    byte=$(dd bs=1 count=1 2>/dev/null | od -An -to1 | tr -d ' ')
    if [ "$byte" = 035 ] && [ "$answer_barriers" = 1 ]; then
      answer
    else
      break
    fi
  done
}}
serve() {{
  byte=$(dd bs=1 count=1 2>/dev/null | od -An -to1 | tr -d ' ')
  [ "$byte" = 035 ] || exit 9
  answer
}}
"#
    )
}

/// The rows of a box `width` cells wide with `title` in its top border and `body` centred in the
/// rows below it, as `excise` draws a dialog.
#[must_use]
pub(crate) fn boxed(title: &str, width: usize, body: &[&str]) -> Vec<String> {
    let tab = format!(" {title} ");
    let mut rows = vec![format!(
        "▟{tab}{}▜",
        "▔".repeat(width - 2 - tab.chars().count())
    )];
    for line in body {
        let padding = width - 2 - line.chars().count();
        rows.push(format!(
            "▏{}{line}{}▕",
            " ".repeat(padding / 2),
            " ".repeat(padding - padding / 2)
        ));
    }
    rows.push("▔".repeat(width));
    rows
}

/// `printf` commands that put `rows` on the screen, the first at `row` and `column` (counted from
/// 1) and the rest below it.
#[must_use]
pub(crate) fn place(row: usize, column: usize, rows: &[String]) -> String {
    let mut script = String::new();
    for (offset, text) in rows.iter().enumerate() {
        writeln!(
            script,
            "printf '\\033[{};{column}H%s' '{text}'",
            row + offset
        )
        .expect("a string takes a write");
    }
    script
}

/// A whole screen: cleared, with a header row, and `dialog` in the middle of it when there is
/// one. The terminal is 120 columns wide and 40 rows high, so the dialog, 100 cells wide, is
/// centred as `excise` centres its own.
#[must_use]
pub(crate) fn screen(header: &str, dialog: Option<(&str, &[&str])>) -> String {
    let mut script = format!("printf '\\033[2J'\n{}", place(1, 1, &[header.to_owned()]));
    if let Some((title, body)) = dialog {
        script.push_str(&place(15, 11, &boxed(title, 100, body)));
    }
    script
}

/// The header of a map whose scan is complete.
pub(crate) const COMPLETE_HEADER: &str = " EXCISE  /scripted  ◆ COMPLETE";
