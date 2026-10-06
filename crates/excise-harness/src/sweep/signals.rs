//! The signals the sweep sends to the builds it runs, and what sending them can leave behind.
//!
//! The F6 row asks what a build does when it is told to end by a signal: SIGTERM, SIGHUP (what
//! closing the terminal sends), and SIGQUIT. A release that predates the signal handling has no
//! handler, so the operating system ends it by the signal's default action, and for SIGQUIT that
//! action is to terminate and dump core. What a dump leaves behind depends on the platform, so
//! which signals the sweep sends does too ([`signals_for`]):
//!
//! * **macOS**: SIGTERM and SIGHUP. A process that a SIGQUIT it does not handle ends is a crash to
//!   macOS, whose crash reporter (`ReportCrash`) writes a report into the person's own
//!   `~/Library/Logs/DiagnosticReports`, whatever the core-file limit is. Every release without a
//!   handler would leave one there, in a folder that is not the sweep's and that it must not clean,
//!   so SIGQUIT is not sent, and the F6 cell on macOS says it was decided from SIGTERM and SIGHUP
//!   ([`skipped_note`]).
//! * **Linux and the other Unix systems**: SIGTERM, SIGHUP, and SIGQUIT, with the soft limit on the
//!   size of a core file at 0 ([`limit_core_dumps`]), so the kernel writes no core file. A machine
//!   that pipes core dumps to a handler (a `kernel.core_pattern` that begins with `|`) does not
//!   enforce that limit (`core(5)`), and what such a handler keeps is its own: the sweep neither
//!   prevents it nor deletes it.
//! * **Windows**: the console is closed. There is no SIGQUIT.

use crate::scenario::Signal;

/// The signals the sweep sends to a build on the platform `os`, spelled as `std::env::consts::OS`
/// spells it: the row's own evidence, less what would make the system write into the person's home.
///
/// Both the checks that send them and the classification that decides the F6 cell from what they
/// found call this with the sweep host's platform, so a signal that is not sent is never required.
pub(crate) fn signals_for(os: &str) -> &'static [Signal] {
    match os {
        "windows" => &[Signal::Close],
        "macos" => &[Signal::Term, Signal::Hup],
        _ => &[Signal::Term, Signal::Hup, Signal::Quit],
    }
}

/// What a cell of the F6 row says about the signal it is not decided from on the platform `os`, when
/// there is one: SIGQUIT on macOS.
pub(crate) fn skipped_note(os: &str) -> Option<&'static str> {
    (os == "macos").then_some(
        "SIGQUIT is not sent on macOS: its default action makes macOS write a crash report into \
         the person's own `~/Library/Logs/DiagnosticReports`",
    )
}

/// Sets the soft limit on the size of a core file to 0, for the sweep and so, by inheritance, for
/// every build it starts.
///
/// It is what lets the sweep send SIGQUIT on Linux: a build with no handler is ended by the default
/// action, which dumps core, and with the limit at 0 the kernel writes no core file (the module
/// documentation says which machines decide for themselves). It does not stop macOS from writing a
/// crash report, which the crash reporter makes and not the core dump, so that is why SIGQUIT is
/// not sent there. The hard limit is left as it is.
#[cfg(unix)]
pub(crate) fn limit_core_dumps() {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};

    if let Ok((_, hard)) = getrlimit(Resource::RLIMIT_CORE) {
        let _ = setrlimit(Resource::RLIMIT_CORE, 0, hard);
    }
}

/// There is no core-file limit to set on Windows, where the sweep closes the console instead of
/// sending SIGQUIT.
#[cfg(not(unix))]
pub(crate) fn limit_core_dumps() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_gets_sigterm_and_sighup_and_no_sigquit() {
        assert_eq!(signals_for("macos"), [Signal::Term, Signal::Hup]);
    }

    #[test]
    fn linux_and_the_other_unix_systems_get_all_three() {
        for os in ["linux", "freebsd", "netbsd", "openbsd", "illumos"] {
            assert_eq!(
                signals_for(os),
                [Signal::Term, Signal::Hup, Signal::Quit],
                "{os}"
            );
        }
    }

    #[test]
    fn windows_gets_the_console_close_and_nothing_else() {
        assert_eq!(signals_for("windows"), [Signal::Close]);
    }

    #[test]
    fn only_macos_says_it_does_not_send_sigquit() {
        let note = skipped_note("macos").expect("macOS does not send SIGQUIT");
        assert!(note.contains("SIGQUIT is not sent on macOS"), "{note}");
        assert!(note.contains("~/Library/Logs/DiagnosticReports"), "{note}");

        for os in ["linux", "windows", "freebsd"] {
            assert_eq!(skipped_note(os), None, "{os}");
        }
    }

    #[test]
    fn a_platform_that_says_it_skips_sigquit_does_not_send_it() {
        for os in ["linux", "macos", "windows", "freebsd"] {
            if skipped_note(os).is_some() {
                assert!(!signals_for(os).contains(&Signal::Quit), "{os}");
            }
        }
    }
}
