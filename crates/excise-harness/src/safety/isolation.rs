//! Profiles and the isolated environment of a spawned `excise`.

use std::ffi::OsString;

use crate::scenario::Profile;

use super::Scratch;

/// The terminal width of the `narrow` profile.
pub const NARROW_COLS: u16 = 60;

/// What a profile changes for a process runner.
///
/// The table is the one in the harness README: `default` changes nothing, `deterministic` asks
/// for reduced motion and one scan thread, `monochrome-ascii` for the monochrome theme and ASCII
/// symbols, `narrow` for a 60-column terminal, and `mouse-keymaps` for mouse input and the Emacs
/// key preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileSettings {
    /// The environment variables the profile sets, on top of the isolation environment.
    pub env: &'static [(&'static str, &'static str)],
    /// The terminal width the profile forces, in place of the scenario's.
    pub cols: Option<u16>,
}

impl ProfileSettings {
    /// The settings of `profile`.
    #[must_use]
    pub const fn for_profile(profile: Profile) -> Self {
        match profile {
            Profile::Default => Self {
                env: &[],
                cols: None,
            },
            Profile::Deterministic => Self {
                env: &[("EXCISE_REDUCED_MOTION", "1"), ("EXCISE_SCAN_THREADS", "1")],
                cols: None,
            },
            Profile::MonochromeAscii => Self {
                env: &[("EXCISE_THEME", "monochrome"), ("EXCISE_ASCII", "1")],
                cols: None,
            },
            Profile::Narrow => Self {
                env: &[],
                cols: Some(NARROW_COLS),
            },
            Profile::MouseKeymaps => Self {
                env: &[("EXCISE_MOUSE", "1"), ("EXCISE_KEYMAP", "emacs")],
                cols: None,
            },
        }
    }
}

/// The complete environment of a spawned `excise`.
///
/// Nothing is inherited. The environment holds the terminal identity (`TERM`, `COLORTERM`,
/// `LANG`), a scratch `HOME` and temporary directory, `EXCISE_CONFIG`, `EXCISE_SCAN_STORE_DIR`,
/// `EXCISE_TEST_EVENTS` when `with_events` is set, and the variables of `profile`. `SHELL` is
/// fixed to `/bin/sh` on Unix because the pseudo-terminal library always adds a `SHELL`, and an
/// inherited one would leak the user's login shell into the run.
///
/// On Windows a process needs a few system variables to start at all, so `SystemRoot`,
/// `SystemDrive`, and `windir` are copied from the parent. That variant is not exercised by this
/// crate's own tests.
#[must_use]
pub fn isolated_env(
    scratch: &Scratch,
    profile: Profile,
    with_events: bool,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = Vec::new();
    let mut set = |name: &str, value: OsString| env.push((OsString::from(name), value));
    set("TERM", "xterm-256color".into());
    set("COLORTERM", "truecolor".into());
    set("LANG", "en_US.UTF-8".into());
    set("HOME", scratch.home().into());
    #[cfg(windows)]
    {
        set("USERPROFILE", scratch.home().into());
        set("TEMP", scratch.tmp().into());
        set("TMP", scratch.tmp().into());
        for name in ["SystemRoot", "SystemDrive", "windir"] {
            if let Some(value) = std::env::var_os(name) {
                set(name, value);
            }
        }
    }
    #[cfg(not(windows))]
    {
        set("TMPDIR", scratch.tmp().into());
        set("SHELL", "/bin/sh".into());
    }
    set("EXCISE_CONFIG", scratch.config_file().into());
    set("EXCISE_SCAN_STORE_DIR", scratch.store().into());
    if with_events {
        set("EXCISE_TEST_EVENTS", scratch.events().into());
    }
    for (name, value) in ProfileSettings::for_profile(profile).env {
        set(name, (*value).into());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(profile: Profile) -> Vec<(String, String)> {
        let scratch = Scratch::create(&std::env::temp_dir()).expect("a scratch directory");
        isolated_env(&scratch, profile, true)
            .into_iter()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect()
    }

    fn value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
        env.iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn the_environment_holds_only_what_the_harness_sets() {
        let env = env_of(Profile::Default);

        for (name, _) in &env {
            assert!(
                matches!(
                    name.as_str(),
                    "TERM"
                        | "COLORTERM"
                        | "LANG"
                        | "HOME"
                        | "TMPDIR"
                        | "SHELL"
                        | "EXCISE_CONFIG"
                        | "EXCISE_SCAN_STORE_DIR"
                        | "EXCISE_TEST_EVENTS"
                        | "USERPROFILE"
                        | "TEMP"
                        | "TMP"
                        | "SystemRoot"
                        | "SystemDrive"
                        | "windir"
                ),
                "unexpected variable {name}"
            );
        }
        assert_eq!(value(&env, "TERM"), Some("xterm-256color"));
        assert!(value(&env, "EXCISE_SCAN_STORE_DIR").is_some_and(|dir| dir.ends_with("store")));
    }

    #[test]
    fn every_scratch_variable_points_inside_the_scratch_area() {
        let scratch = Scratch::create(&std::env::temp_dir()).expect("a scratch directory");
        let env = isolated_env(&scratch, Profile::Default, true);

        for (name, value) in &env {
            let name = name.to_string_lossy();
            if matches!(
                name.as_ref(),
                "HOME"
                    | "TMPDIR"
                    | "EXCISE_CONFIG"
                    | "EXCISE_SCAN_STORE_DIR"
                    | "EXCISE_TEST_EVENTS"
            ) {
                assert!(
                    std::path::Path::new(value).starts_with(scratch.root()),
                    "{name} = {value:?} is outside {:?}",
                    scratch.root()
                );
            }
        }
    }

    #[test]
    fn the_event_file_is_only_requested_when_asked_for() {
        let scratch = Scratch::create(&std::env::temp_dir()).expect("a scratch directory");

        let without = isolated_env(&scratch, Profile::Default, false);

        assert!(without.iter().all(|(name, _)| name != "EXCISE_TEST_EVENTS"));
    }

    #[test]
    fn profiles_add_exactly_their_documented_variables() {
        assert_eq!(
            value(&env_of(Profile::Default), "EXCISE_REDUCED_MOTION"),
            None
        );

        let deterministic = env_of(Profile::Deterministic);
        assert_eq!(value(&deterministic, "EXCISE_REDUCED_MOTION"), Some("1"));
        assert_eq!(value(&deterministic, "EXCISE_SCAN_THREADS"), Some("1"));

        let monochrome = env_of(Profile::MonochromeAscii);
        assert_eq!(value(&monochrome, "EXCISE_THEME"), Some("monochrome"));
        assert_eq!(value(&monochrome, "EXCISE_ASCII"), Some("1"));

        let mouse = env_of(Profile::MouseKeymaps);
        assert_eq!(value(&mouse, "EXCISE_MOUSE"), Some("1"));
        assert_eq!(value(&mouse, "EXCISE_KEYMAP"), Some("emacs"));

        assert_eq!(value(&env_of(Profile::Narrow), "EXCISE_MOUSE"), None);
    }

    #[test]
    fn only_the_narrow_profile_forces_the_terminal_width() {
        for profile in Profile::ALL {
            let expected = (*profile == Profile::Narrow).then_some(NARROW_COLS);
            assert_eq!(
                ProfileSettings::for_profile(*profile).cols,
                expected,
                "{profile}"
            );
        }
    }
}
