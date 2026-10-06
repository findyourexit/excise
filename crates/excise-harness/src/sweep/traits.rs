//! What the sweep asks of a build before it checks anything: the flags its `--help` lists, which is
//! what the checks adapt to.
//!
//! Every published version takes `--format json --output FILE ROOT`, but the sweep does not assume
//! that of a version it has not asked: a build whose `--help` lists no `--format` and `--output`
//! has no headless check, and says so. Where a build keeps its scan data is the other thing that
//! differs: only v1.3.0 and later take `--scan-store-dir` (and the `EXCISE_SCAN_STORE_DIR` the
//! isolated environment sets); an earlier build keeps its scratch in the temporary directory, which
//! the isolated environment also points into the scratch area, so the residue checks look there.

use std::{path::Path, process::Command, time::Duration};

use crate::{
    headless::process,
    report::SweepTraits,
    runner::resolve_binary,
    safety::{Scratch, isolated_env},
    scenario::Profile,
};

/// How long `--help` may take.
const HELP_BOUND: Duration = Duration::from_secs(20);

/// What a build's `--help` says it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Traits {
    /// `--format`: the build can write a report instead of drawing the interface.
    pub format: bool,
    /// `--output`: and can write it to a file.
    pub output: bool,
    /// `--scan-store-dir`: the build keeps its scan data in the directory the harness names.
    pub scan_store_dir: bool,
}

impl Traits {
    /// Whether the build takes `--format json --output FILE`.
    pub(crate) const fn headless(self) -> bool {
        self.format && self.output
    }

    /// The part of the traits the document records.
    pub(crate) const fn recorded(self, report_version: Option<u32>) -> SweepTraits {
        SweepTraits {
            report_version,
            scan_store_dir: self.scan_store_dir,
        }
    }
}

/// Reads the flags out of `help`: a flag is a whole word that starts with two dashes.
pub(crate) fn from_help(help: &str) -> Traits {
    let has = |flag: &str| {
        help.split(|character: char| !(character.is_ascii_alphanumeric() || character == '-'))
            .any(|word| word == flag)
    };
    Traits {
        format: has("--format"),
        output: has("--output"),
        scan_store_dir: has("--scan-store-dir"),
    }
}

/// Asks the build at `binary` for its `--help`, in the isolated environment, and reads the flags.
///
/// # Errors
///
/// Returns why the build could not be asked: it cannot be started, or it does not answer `--help`
/// with exit code 0 and some text.
pub(crate) fn probe(binary: &Path, work_dir: &Path) -> Result<Traits, String> {
    let program = resolve_binary(binary).map_err(|error| error.to_string())?;
    let scratch = Scratch::create(work_dir).map_err(|error| error.to_string())?;
    let mut command = Command::new(&program);
    command
        .arg("--help")
        .env_clear()
        .envs(isolated_env(&scratch, Profile::Default, false, None))
        .current_dir(scratch.cwd());
    let finished =
        process::run(&mut command, HELP_BOUND, false, false).map_err(|error| error.to_string())?;
    if finished.timed_out || finished.ended.code() != Some(0) || finished.stdout.head.is_empty() {
        return Err(format!(
            "`--help` {}",
            if finished.timed_out {
                "did not answer in time".to_owned()
            } else {
                format!(
                    "{} and printed {} bytes",
                    finished.ended.describe(),
                    finished.stdout.bytes
                )
            }
        ));
    }
    Ok(from_help(&String::from_utf8_lossy(&finished.stdout.head)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The options of v1.0.0's `--help`, which the later builds only add to.
    const V1_0_0: &str = "\
Usage: excise [OPTIONS] [ROOT]

Arguments:
  [ROOT]  Directory to scan [default: .]

Options:
      --config <CONFIG>            Path to a TOML configuration file [env: EXCISE_CONFIG=]
      --format <FORMAT>            Output format [env: EXCISE_FORMAT=] [possible values: tui, json]
      --output <OUTPUT>            Write the report to a file [env: EXCISE_OUTPUT=]
      --reduced-motion             Reduce animation [env: EXCISE_REDUCED_MOTION=]
      --scan-threads <N>           Scanner threads [env: EXCISE_SCAN_THREADS=]
  -h, --help                       Print help
  -V, --version                    Print version
";

    #[test]
    fn a_build_that_predates_the_scan_store_has_none_to_name() {
        let traits = from_help(V1_0_0);

        assert!(traits.headless());
        assert!(!traits.scan_store_dir);
    }

    #[test]
    fn a_build_with_the_scan_store_names_its_directory() {
        let help = format!(
            "{V1_0_0}      --scan-store-dir <DIR>       Scan-store directory [env: EXCISE_SCAN_STORE_DIR=]\n"
        );

        let traits = from_help(&help);

        assert!(traits.headless());
        assert!(traits.scan_store_dir);
    }

    #[test]
    fn a_flag_is_a_whole_word_and_prose_about_one_is_not_a_flag() {
        assert!(!from_help("Use --formatted output, or --output-dir").headless());
        assert!(!from_help("see --scan-store-dir-less builds").scan_store_dir);
        assert!(from_help("--format=json --output=x").headless());
    }

    #[test]
    fn a_build_without_a_headless_report_has_no_headless_check() {
        let traits = from_help("Usage: excise [ROOT]\n      --theme <THEME>\n");

        assert!(!traits.headless());
        assert!(!traits.format && !traits.output);
    }

    #[test]
    fn the_document_records_the_report_version_and_where_scan_data_lives() {
        let recorded = from_help(V1_0_0).recorded(Some(1));

        assert_eq!(recorded.report_version, Some(1));
        assert!(!recorded.scan_store_dir);
    }
}
