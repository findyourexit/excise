//! `cargo xtask tui`: an interactive session driver for exploring `excise` on a fixture.
//!
//! This is a thin wrapper. Sessions, keys, the screen, events, deletions, and the output document
//! all live in `excise_harness::tui`; this file parses the command line, finds or builds the
//! release binary for `open`, and prints the one document the command produces. Nothing else is
//! ever written to stdout, a failure is a document too, and the exit status is 0 for success, 2
//! for a command line that is not valid, and 1 for any other failure.
//!
//! The command `supervise <session directory>` is how a session's supervisor process is started
//! (the harness runs this same binary with it); it is internal and prints nothing.

use std::{
    env,
    error::Error,
    ffi::OsString,
    io::{self, Write as _},
    path::{Path, PathBuf},
    process,
};

use excise_harness::{
    report::{
        Document as _,
        tui::{TuiCommand, TuiError, TuiErrorKind},
    },
    scenario::{EntryKind, Profile},
    tui::{
        Command, DEFAULT_DELETE_TIMEOUT, DEFAULT_IDLE_TIMEOUT, DEFAULT_KEYS_TIMEOUT, DEFAULT_SIZE,
        Launcher, OpenRequest, Options, Outcome, STATE_DIR_NAME, SUPPORTED, check_fixture, execute,
        parse_duration, parse_size, serve,
    },
};

use crate::e2e::{BINARY_ENV, build_release_binary};

const USAGE: &str = "usage: cargo xtask tui <command>
  open --fixture <ID> [--profile <NAME>] [--size <COLS>x<ROWS>] [--record] [--idle-timeout <DURATION>]
  keys <SESSION> <KEY>... [--timeout <DURATION>]
  delete <SESSION> --name <FIXTURE-RELATIVE PATH> --kind folder|file [--timeout <DURATION>]
  screen <SESSION>
  events <SESSION> [--since <N>]
  close <SESSION>
  list
a <KEY> is a key name (enter esc backspace tab up down left right page_up page_down), one \
character, ctrl+ or alt+ before either, or type:<TEXT>; a <DURATION> is 250ms, 30s, 15m, or 2h";

/// A command line, parsed. Parsing makes nothing and sends nothing.
#[derive(Debug, PartialEq, Eq)]
enum Parsed {
    /// Start the supervisor of the session whose directory this is.
    Supervise(PathBuf),
    /// Open a session; the binary is found afterwards.
    Open(OpenArguments),
    /// Any other command.
    Run(Command),
}

#[derive(Debug, PartialEq, Eq)]
struct OpenArguments {
    fixture: String,
    profile: Profile,
    size: (u16, u16),
    record: bool,
    idle_timeout: std::time::Duration,
}

/// A command line that is not valid, and the command it was for, when it named one.
#[derive(Debug, PartialEq, Eq)]
struct UsageError {
    command: Option<TuiCommand>,
    message: String,
}

/// Runs the command. On success it returns; on any failure it has printed the failure's document
/// and exits with its status.
pub fn tui(args: impl Iterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    let arguments = match utf8_arguments(args) {
        Ok(arguments) => arguments,
        Err(error) => finish(&usage_failure(&error)),
    };
    let parsed = match parse(&arguments) {
        Ok(parsed) => parsed,
        Err(error) => finish(&usage_failure(&error)),
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| io::Error::other("the xtask manifest has no parent directory"))?
        .to_path_buf();
    // A relative target directory is the one the release build below finds, which runs in `root`.
    let target =
        env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), |dir| root.join(dir));

    let command = match parsed {
        Parsed::Supervise(directory) => {
            return serve(&directory).map_err(Into::into);
        }
        Parsed::Run(command) => command,
        Parsed::Open(open) => {
            // Where the driver does not run, `execute` fails the command with the platform's
            // name, so there is neither a fixture to check nor anything to build.
            if SUPPORTED {
                // A fixture id that is wrong is reported at once, before a build is started for it.
                if let Err(error) = check_fixture(&open.fixture) {
                    finish(&Outcome::failure(Some(TuiCommand::Open), None, error));
                }
            }
            let binary = if SUPPORTED {
                resolve_binary(&root, &target)
            } else {
                Ok(PathBuf::new())
            };
            match binary {
                Ok(binary) => Command::Open(OpenRequest {
                    fixture: open.fixture,
                    profile: open.profile,
                    size: open.size,
                    record: open.record,
                    idle_timeout: open.idle_timeout,
                    binary,
                }),
                Err(error) => finish(&Outcome::failure(
                    Some(TuiCommand::Open),
                    None,
                    TuiError::new(TuiErrorKind::Failed, error.to_string()),
                )),
            }
        }
    };
    // Only `open` starts a supervisor, which is this program again.
    let program = match env::current_exe() {
        Ok(program) => program,
        Err(error) if matches!(command, Command::Open(_)) => finish(&Outcome::failure(
            Some(TuiCommand::Open),
            None,
            TuiError::new(
                TuiErrorKind::Failed,
                format!("cannot find this program, which starts the session's supervisor: {error}"),
            ),
        )),
        Err(_) => PathBuf::new(),
    };
    let options = Options {
        state_dir: target.join(STATE_DIR_NAME),
        launcher: Launcher {
            program,
            args: vec!["tui".into(), "supervise".into()],
        },
    };
    finish(&execute(&options, &command))
}

/// The arguments as text. A name or a key that is not text cannot be sent to a session or looked
/// up in a fixture, and a command that cannot be read is a usage error like any other.
fn utf8_arguments(args: impl Iterator<Item = OsString>) -> Result<Vec<String>, UsageError> {
    args.map(|argument| {
        argument.into_string().map_err(|argument| UsageError {
            command: None,
            message: format!(
                "the argument `{}` is not valid UTF-8",
                argument.to_string_lossy()
            ),
        })
    })
    .collect()
}

/// The document of a command line that is not valid.
fn usage_failure(error: &UsageError) -> Outcome {
    Outcome::failure(
        error.command,
        None,
        TuiError::new(TuiErrorKind::Usage, format!("{}\n{USAGE}", error.message)),
    )
}

/// Prints the command's document, the only thing a command writes to stdout, and ends the
/// process with the status the document calls for.
fn finish(outcome: &Outcome) -> ! {
    let mut stdout = io::stdout().lock();
    let written = match outcome.document.to_json_pretty() {
        Ok(text) => stdout
            .write_all(text.as_bytes())
            .and_then(|()| stdout.flush()),
        Err(error) => Err(io::Error::other(error)),
    };
    if let Err(error) = written {
        eprintln!("cannot print the document: {error}");
        process::exit(1);
    }
    process::exit(i32::from(outcome.exit_code()))
}

/// The `excise` binary a session runs: the one `EXCISE_E2E_BINARY` names, or a release build.
fn resolve_binary(root: &Path, target: &Path) -> Result<PathBuf, Box<dyn Error>> {
    match env::var_os(BINARY_ENV).filter(|path| !path.is_empty()) {
        Some(path) => Ok(PathBuf::from(path)),
        None => build_release_binary(root, target),
    }
}

fn parse(arguments: &[String]) -> Result<Parsed, UsageError> {
    let Some((name, rest)) = arguments.split_first() else {
        return Err(UsageError {
            command: None,
            message: "name a command".to_owned(),
        });
    };
    let usage = |command: Option<TuiCommand>, message: String| UsageError { command, message };
    match name.as_str() {
        "supervise" => match rest {
            [directory] => Ok(Parsed::Supervise(PathBuf::from(directory))),
            _ => Err(usage(
                None,
                "`supervise` takes one session directory".to_owned(),
            )),
        },
        "open" => parse_open(rest)
            .map(Parsed::Open)
            .map_err(|message| usage(Some(TuiCommand::Open), message)),
        "keys" => parse_keys(rest)
            .map(Parsed::Run)
            .map_err(|message| usage(Some(TuiCommand::Keys), message)),
        "delete" => parse_delete(rest)
            .map(Parsed::Run)
            .map_err(|message| usage(Some(TuiCommand::Delete), message)),
        "screen" => session_only(rest, "screen")
            .map(|session| Parsed::Run(Command::Screen { session }))
            .map_err(|message| usage(Some(TuiCommand::Screen), message)),
        "close" => session_only(rest, "close")
            .map(|session| Parsed::Run(Command::Close { session }))
            .map_err(|message| usage(Some(TuiCommand::Close), message)),
        "events" => parse_events(rest)
            .map(Parsed::Run)
            .map_err(|message| usage(Some(TuiCommand::Events), message)),
        "list" if rest.is_empty() => Ok(Parsed::Run(Command::List)),
        "list" => Err(usage(
            Some(TuiCommand::List),
            "`list` takes no arguments".to_owned(),
        )),
        other => Err(usage(None, format!("unknown command `{other}`"))),
    }
}

/// The value of the option `flag`, which must come once.
fn take_value(
    arguments: &mut impl Iterator<Item = String>,
    flag: &str,
    slot: &mut Option<String>,
) -> Result<(), String> {
    let value = arguments
        .next()
        .ok_or_else(|| format!("`{flag}` needs a value"))?;
    if slot.replace(value).is_some() {
        return Err(format!("`{flag}` is given twice"));
    }
    Ok(())
}

fn parse_open(arguments: &[String]) -> Result<OpenArguments, String> {
    let mut arguments = arguments.iter().cloned();
    let (mut fixture, mut profile, mut size, mut idle) = (None, None, None, None);
    let mut record = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--fixture" => take_value(&mut arguments, "--fixture", &mut fixture)?,
            "--profile" => take_value(&mut arguments, "--profile", &mut profile)?,
            "--size" => take_value(&mut arguments, "--size", &mut size)?,
            "--idle-timeout" => take_value(&mut arguments, "--idle-timeout", &mut idle)?,
            "--record" => record = true,
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    let fixture = fixture.ok_or_else(|| {
        "`open` needs `--fixture <ID>`: the id of a fixture spec, never a path".to_owned()
    })?;
    let profile = match profile {
        None => Profile::Default,
        Some(text) => Profile::ALL
            .iter()
            .copied()
            .find(|profile| profile.as_str() == text)
            .ok_or_else(|| {
                let known: Vec<&str> = Profile::ALL
                    .iter()
                    .map(|profile| profile.as_str())
                    .collect();
                format!(
                    "unknown profile `{text}`; the profiles are {}",
                    known.join(", ")
                )
            })?,
    };
    Ok(OpenArguments {
        fixture,
        profile,
        size: size.map_or(Ok(DEFAULT_SIZE), |text| parse_size(&text))?,
        record,
        idle_timeout: idle.map_or(Ok(DEFAULT_IDLE_TIMEOUT), |text| parse_duration(&text))?,
    })
}

/// A session and nothing else.
fn session_only(arguments: &[String], command: &str) -> Result<String, String> {
    match arguments {
        [session] if !session.starts_with("--") => Ok(session.clone()),
        _ => Err(format!("`{command}` takes one session id")),
    }
}

fn parse_keys(arguments: &[String]) -> Result<Command, String> {
    let mut arguments = arguments.iter().cloned();
    let mut timeout = None;
    let mut positional = Vec::new();
    while let Some(argument) = arguments.next() {
        if argument == "--timeout" {
            take_value(&mut arguments, "--timeout", &mut timeout)?;
        } else {
            positional.push(argument);
        }
    }
    let mut positional = positional.into_iter();
    let session = positional
        .next()
        .ok_or_else(|| "`keys` needs a session id and the keys to send".to_owned())?;
    let keys: Vec<String> = positional.collect();
    if keys.is_empty() {
        return Err("`keys` needs at least one key to send".to_owned());
    }
    Ok(Command::Keys {
        session,
        keys,
        timeout: timeout.map_or(Ok(DEFAULT_KEYS_TIMEOUT), |text| parse_duration(&text))?,
    })
}

fn parse_delete(arguments: &[String]) -> Result<Command, String> {
    let mut arguments = arguments.iter().cloned();
    let (mut name, mut kind, mut timeout) = (None, None, None);
    let mut session = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--name" => take_value(&mut arguments, "--name", &mut name)?,
            "--kind" => take_value(&mut arguments, "--kind", &mut kind)?,
            "--timeout" => take_value(&mut arguments, "--timeout", &mut timeout)?,
            flag if flag.starts_with("--") => return Err(format!("unknown argument `{flag}`")),
            _ if session.is_none() => session = Some(argument),
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    let session = session.ok_or_else(|| "`delete` needs a session id".to_owned())?;
    let name = name.ok_or_else(|| {
        "`delete` needs `--name <FIXTURE-RELATIVE PATH>`, for example `--name victim`".to_owned()
    })?;
    let kind = match kind.as_deref() {
        Some("folder") => EntryKind::Folder,
        Some("file") => EntryKind::File,
        Some(other) => return Err(format!("`--kind` is `folder` or `file`, not `{other}`")),
        None => return Err("`delete` needs `--kind folder|file`".to_owned()),
    };
    Ok(Command::Delete {
        session,
        name,
        kind,
        timeout: timeout.map_or(Ok(DEFAULT_DELETE_TIMEOUT), |text| parse_duration(&text))?,
    })
}

fn parse_events(arguments: &[String]) -> Result<Command, String> {
    let mut arguments = arguments.iter().cloned();
    let (mut since, mut session) = (None, None);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--since" => take_value(&mut arguments, "--since", &mut since)?,
            flag if flag.starts_with("--") => return Err(format!("unknown argument `{flag}`")),
            _ if session.is_none() => session = Some(argument),
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    let since = since
        .map(|text| {
            text.parse::<u64>()
                .map_err(|_| format!("`--since` takes an event index, not `{text}`"))
        })
        .transpose()?;
    Ok(Command::Events {
        session: session.ok_or_else(|| "`events` needs a session id".to_owned())?,
        since,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn parsed(arguments: &[&str]) -> Result<Parsed, UsageError> {
        parse(
            &arguments
                .iter()
                .map(|text| (*text).to_owned())
                .collect::<Vec<_>>(),
        )
    }

    fn message(arguments: &[&str]) -> String {
        parsed(arguments).expect_err("a usage error").message
    }

    #[test]
    fn open_takes_a_fixture_and_defaults_the_rest() {
        assert_eq!(
            parsed(&["open", "--fixture", "delete-file"]),
            Ok(Parsed::Open(OpenArguments {
                fixture: "delete-file".to_owned(),
                profile: Profile::Default,
                size: DEFAULT_SIZE,
                record: false,
                idle_timeout: DEFAULT_IDLE_TIMEOUT,
            }))
        );
        assert_eq!(
            parsed(&[
                "open",
                "--fixture",
                "delete-file",
                "--profile",
                "narrow",
                "--size",
                "90x30",
                "--record",
                "--idle-timeout",
                "30s",
            ]),
            Ok(Parsed::Open(OpenArguments {
                fixture: "delete-file".to_owned(),
                profile: Profile::Narrow,
                size: (90, 30),
                record: true,
                idle_timeout: Duration::from_secs(30),
            }))
        );
    }

    #[test]
    fn open_refuses_what_is_not_a_fixture_id_option() {
        assert!(message(&["open"]).contains("--fixture"));
        assert!(message(&["open", "--fixture"]).contains("needs a value"));
        assert!(message(&["open", "--fixture", "a", "--fixture", "b"]).contains("twice"));
        assert!(
            message(&["open", "--fixture", "a", "--root", "/"])
                .contains("unknown argument `--root`")
        );
        assert!(
            message(&["open", "--fixture", "a", "/home/me"])
                .contains("unknown argument `/home/me`")
        );
        assert!(
            message(&["open", "--fixture", "a", "--profile", "x"]).contains("the profiles are")
        );
        assert!(message(&["open", "--fixture", "a", "--size", "10x4"]).contains("smaller"));
        assert!(
            message(&["open", "--fixture", "a", "--idle-timeout", "soon"]).contains("duration")
        );
    }

    #[test]
    fn keys_takes_a_session_and_keys_in_any_order_with_the_timeout_anywhere() {
        assert_eq!(
            parsed(&["keys", "0123abcd", "down", "-", "type:--timeout", "enter"]),
            Ok(Parsed::Run(Command::Keys {
                session: "0123abcd".to_owned(),
                keys: ["down", "-", "type:--timeout", "enter"]
                    .map(str::to_owned)
                    .to_vec(),
                timeout: DEFAULT_KEYS_TIMEOUT,
            }))
        );
        assert_eq!(
            parsed(&["keys", "--timeout", "5s", "0123abcd", "y"]),
            Ok(Parsed::Run(Command::Keys {
                session: "0123abcd".to_owned(),
                keys: vec!["y".to_owned()],
                timeout: Duration::from_secs(5),
            }))
        );
        assert!(message(&["keys"]).contains("session id"));
        assert!(message(&["keys", "0123abcd"]).contains("at least one key"));
        assert!(message(&["keys", "0123abcd", "--timeout"]).contains("needs a value"));
    }

    #[test]
    fn delete_needs_a_name_and_a_kind() {
        assert_eq!(
            parsed(&[
                "delete",
                "0123abcd",
                "--name",
                "docs/keep-1.txt",
                "--kind",
                "file"
            ]),
            Ok(Parsed::Run(Command::Delete {
                session: "0123abcd".to_owned(),
                name: "docs/keep-1.txt".to_owned(),
                kind: EntryKind::File,
                timeout: DEFAULT_DELETE_TIMEOUT,
            }))
        );
        assert!(message(&["delete", "0123abcd", "--kind", "file"]).contains("--name"));
        assert!(message(&["delete", "0123abcd", "--name", "x"]).contains("--kind"));
        assert!(
            message(&["delete", "0123abcd", "--name", "x", "--kind", "item"])
                .contains("`folder` or `file`")
        );
        assert!(message(&["delete", "--name", "x", "--kind", "file"]).contains("session id"));
        assert!(
            message(&["delete", "a", "b", "--name", "x", "--kind", "file"])
                .contains("unexpected argument `b`")
        );
    }

    #[test]
    fn the_read_only_commands_take_a_session_and_little_else() {
        assert_eq!(
            parsed(&["screen", "0123abcd"]),
            Ok(Parsed::Run(Command::Screen {
                session: "0123abcd".to_owned()
            }))
        );
        assert_eq!(
            parsed(&["close", "0123abcd"]),
            Ok(Parsed::Run(Command::Close {
                session: "0123abcd".to_owned()
            }))
        );
        assert_eq!(
            parsed(&["events", "0123abcd", "--since", "7"]),
            Ok(Parsed::Run(Command::Events {
                session: "0123abcd".to_owned(),
                since: Some(7)
            }))
        );
        assert_eq!(parsed(&["list"]), Ok(Parsed::Run(Command::List)));
        assert!(message(&["screen"]).contains("one session id"));
        assert!(message(&["screen", "a", "b"]).contains("one session id"));
        assert!(message(&["events", "0123abcd", "--since", "x"]).contains("event index"));
        assert!(message(&["list", "now"]).contains("no arguments"));
    }

    #[test]
    fn an_unknown_command_or_none_is_a_usage_error_without_a_command() {
        assert_eq!(
            parsed(&[]),
            Err(UsageError {
                command: None,
                message: "name a command".to_owned()
            })
        );
        assert_eq!(parsed(&["bogus"]).expect_err("unknown").command, None);
        assert_eq!(
            parsed(&["keys"]).expect_err("usage").command,
            Some(TuiCommand::Keys)
        );
    }

    #[test]
    fn the_supervisor_command_takes_one_directory() {
        assert_eq!(
            parsed(&["supervise", "/x/excise-tui/0123abcd"]),
            Ok(Parsed::Supervise(PathBuf::from("/x/excise-tui/0123abcd")))
        );
        assert!(message(&["supervise"]).contains("one session directory"));
    }

    #[test]
    fn an_argument_that_is_not_text_is_a_usage_error_not_a_panic() {
        assert_eq!(
            utf8_arguments(["keys", "0123abcd", "x"].map(OsString::from).into_iter()),
            Ok(vec![
                "keys".to_owned(),
                "0123abcd".to_owned(),
                "x".to_owned()
            ])
        );

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;

            let arguments = [OsString::from("keys"), OsString::from_vec(vec![b'k', 0xff])];
            let error = utf8_arguments(arguments.into_iter()).expect_err("not text");
            assert_eq!(error.command, None);
            assert!(
                error.message.contains("not valid UTF-8"),
                "{}",
                error.message
            );
            let document = usage_failure(&error).document;
            assert_eq!(document.command, None);
            assert!(!document.ok);
        }
    }
}
