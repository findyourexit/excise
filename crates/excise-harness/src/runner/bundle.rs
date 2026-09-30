//! Failure bundles: the evidence for one failed scenario.
//!
//! A bundle is a directory holding
//!
//! * `failure.json`: the `harness-failure` document;
//! * `session.cast`: the asciicast recording of the session, with timestamped output (`"o"`) and
//!   input (`"i"`) events;
//! * `screen.txt`: what failed, the terminal modes, and the screen text at that moment;
//! * `events.jsonl`: a copy of the event file;
//! * `repro.txt`: how to rerun the scenario, and the exact `excise` invocation with its whole
//!   environment.
//!
//! The fixture itself is kept only when the run asked for it (`--keep-fixture`); the fixture hash and
//! seed in the document say which fixture it was.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use crate::{
    pty::TerminalModes,
    report::{
        Document, FailedStep, FailureKind, FixtureIdentity, HarnessFailure, Rusage, SchemaVersion,
        ScreenComparison, TerminalModes as ReportModes,
    },
    scenario::Profile,
};

use super::outcome::StepFailure;

/// The exact command line of the process that ran.
#[derive(Debug, Clone)]
pub(crate) struct Invocation {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

impl Invocation {
    /// The invocation as one POSIX shell command that clears the environment first.
    pub(crate) fn shell_command(&self) -> String {
        let mut command = format!("cd {} && env -i", shell_quote(&self.cwd.to_string_lossy()));
        for (name, value) in &self.env {
            command.push(' ');
            command.push_str(&shell_quote(&format!("{name}={value}")));
        }
        command.push(' ');
        command.push_str(&shell_quote(&self.program.to_string_lossy()));
        for argument in &self.args {
            command.push(' ');
            command.push_str(&shell_quote(argument));
        }
        command
    }
}

/// Everything a bundle records.
pub(crate) struct Bundle<'a> {
    pub scenario: &'a str,
    pub profile: Profile,
    pub failure: &'a StepFailure,
    pub screen_text: &'a str,
    pub screen_size: (u16, u16),
    pub cursor: (u16, u16),
    pub modes: TerminalModes,
    pub recording: &'a Path,
    pub events: &'a Path,
    pub rusage: Rusage,
    pub fixture: FixtureIdentity,
    pub repro_command: &'a str,
    pub invocation: &'a Invocation,
}

/// Writes the bundle into `dir`, which is created if it does not exist.
///
/// # Errors
///
/// Returns an error if a file cannot be written, or if the failure document cannot be rendered.
pub(crate) fn write(dir: &Path, bundle: &Bundle<'_>) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let cast = dir.join("session.cast");
    if bundle.recording.exists() {
        fs::copy(bundle.recording, &cast)?;
    } else {
        fs::write(&cast, "")?;
    }
    if bundle.events.exists() {
        fs::copy(bundle.events, dir.join("events.jsonl"))?;
    }
    fs::write(dir.join("screen.txt"), screen_dump(bundle))?;
    fs::write(dir.join("repro.txt"), repro(bundle))?;

    let modes = bundle.modes;
    let document = HarnessFailure {
        document_kind: FailureKind::default(),
        schema_version: SchemaVersion,
        scenario: bundle.scenario.to_owned(),
        profile: bundle.profile,
        failed_step: FailedStep {
            index: u32::try_from(bundle.failure.index).unwrap_or(u32::MAX),
            description: format!(
                "{} [{:?}]: {}",
                bundle.failure.description,
                bundle.failure.cause,
                first_line(&bundle.failure.detail)
            ),
        },
        screen: ScreenComparison {
            expected: bundle.failure.expected.clone(),
            actual: bundle.screen_text.to_owned(),
        },
        // A platform that cannot report a line-discipline mode reports the cooked default.
        terminal_modes: ReportModes {
            alternate_screen: modes.alternate_screen,
            cursor_visible: modes.cursor_visible,
            echo: modes.echo.unwrap_or(true),
            icanon: modes.icanon.unwrap_or(true),
        },
        cast_path: cast.to_string_lossy().into_owned(),
        rusage: bundle.rusage,
        fixture: bundle.fixture.clone(),
        repro_command: bundle.repro_command.to_owned(),
    };
    let json = document.to_json_pretty().map_err(|error| {
        io::Error::other(format!("cannot render the failure document: {error}"))
    })?;
    fs::write(dir.join("failure.json"), json)
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

fn screen_dump(bundle: &Bundle<'_>) -> String {
    let modes = bundle.modes;
    let mode = |value: Option<bool>| match value {
        Some(true) => "on",
        Some(false) => "off",
        None => "unknown",
    };
    format!(
        "scenario {}, profile {}\n\
         failed step {}: {}\n\
         expected: {}\n\
         found: {}\n\
         \n\
         terminal {}x{}, cursor at row {} column {}\n\
         alternate screen: {}, cursor: {}, echo: {}, canonical mode: {}\n\
         \n\
         --- screen ---\n\
         {}\n",
        bundle.scenario,
        bundle.profile,
        bundle.failure.index,
        bundle.failure.description,
        bundle.failure.expected,
        bundle.failure.detail,
        bundle.screen_size.1,
        bundle.screen_size.0,
        bundle.cursor.0,
        bundle.cursor.1,
        mode(Some(modes.alternate_screen)),
        if modes.cursor_visible {
            "visible"
        } else {
            "hidden"
        },
        mode(modes.echo),
        mode(modes.icanon),
        bundle.screen_text,
    )
}

fn repro(bundle: &Bundle<'_>) -> String {
    format!(
        "Rerun this scenario through the harness. The flag keeps the fixture and the scratch\n\
         directory so that the invocation below still has something to run against:\n\
         \n\
         \x20   {}\n\
         \n\
         The exact `excise` invocation of the failed run, with its whole environment (POSIX shell;\n\
         run it in a terminal, and only against the kept fixture):\n\
         \n\
         \x20   {}\n",
        bundle.repro_command,
        bundle.invocation.shell_command(),
    )
}

/// Quotes `text` for a POSIX shell, unless it needs no quoting.
fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_words_are_left_alone_and_everything_else_is_single_quoted() {
        assert_eq!(shell_quote("/tmp/xh-1/fx"), "/tmp/xh-1/fx");
        assert_eq!(shell_quote("TERM=xterm-256color"), "TERM=xterm-256color");
        assert_eq!(shell_quote("has space"), "'has space'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$HOME;rm"), "'$HOME;rm'");
    }

    #[test]
    fn the_invocation_starts_from_an_empty_environment_in_the_working_directory() {
        let invocation = Invocation {
            program: PathBuf::from("/opt/excise/bin/excise"),
            args: vec!["/tmp/xh-1/fx".to_owned()],
            env: vec![
                ("TERM".to_owned(), "xterm-256color".to_owned()),
                ("HOME".to_owned(), "/tmp/scratch dir/home".to_owned()),
            ],
            cwd: PathBuf::from("/tmp/scratch dir/cwd"),
        };

        assert_eq!(
            invocation.shell_command(),
            "cd '/tmp/scratch dir/cwd' && env -i TERM=xterm-256color 'HOME=/tmp/scratch dir/home' \
             /opt/excise/bin/excise /tmp/xh-1/fx"
        );
    }
}
