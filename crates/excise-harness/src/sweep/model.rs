//! What the sweep gathers, in the form the classification reads it.

use std::collections::BTreeMap;

use crate::report::{SweepMeasurement, SweepTier};

use super::{
    checks::{
        Checked, DescriptorObservation, DriftObservation, FilterObservation, IdleObservation,
        KillRestartObservation, SignalObservation,
    },
    headless::OracleObservation,
    traits::Traits,
};

/// The fixtures the checks play their roles on. A row names the fixture its evidence comes from,
/// and the cell says so when that fixture was not run.
pub(crate) mod roles {
    /// The scan-starvation repro tree of F1 and F2: 2,390 entries, nested four levels.
    pub const TIMING: &str = "node-modules-2k";
    /// The hostile class: unreadable folders (F10).
    pub const HOSTILE: &str = "hostile-small";
    /// A chain deeper than `PATH_MAX` (F11).
    pub const DEEP: &str = "deep-past-path-max";
    /// A tiny tree for the checks that only need a scan that completes at once (F6, F4, F3).
    pub const QUICK: &str = "navigate-folders";
    /// `victim/`, the largest entry, which holds folders called `part00` (F22, F23b).
    pub const FILTER: &str = "delete-folder";
    /// The file that is the provisional largest entry until the tail of the walk (F5).
    pub const DRIFT: &str = "selection-drift";
    /// A flat tree of 1,002 entries: the descriptor baseline (F13).
    pub const DESCRIPTORS_SMALL: &str = "wide-1k";
    /// A tree of 49,050 entries in 49 folders (F13).
    pub const DESCRIPTORS_LARGE: &str = "tiny-files-50k";
}

/// What a sweep runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    /// How much of the sweep runs.
    pub tier: SweepTier,
    /// The fixtures every version is scanned on and held to the oracle.
    pub oracle_fixtures: Vec<String>,
    /// The fixtures whose scans and interface runs are timed in paired rounds.
    pub timing_fixtures: Vec<String>,
}

impl Plan {
    /// Whether the checks that need large fixtures run: the full tier only.
    pub(crate) fn is_full(&self) -> bool {
        self.tier == SweepTier::Full
    }
}

/// A check's result, and where its evidence is in the document and on disk.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Placed<T> {
    /// What the check came to.
    pub checked: Checked<T>,
    /// A JSON pointer to its entry in the document's `checks`.
    pub pointer: String,
    /// The evidence file, as a path below the run's directory.
    pub evidence: Option<String>,
}

impl<T> Placed<T> {
    /// The places a cell can point a reader to.
    pub(crate) fn evidence_paths(&self) -> Vec<String> {
        self.evidence
            .iter()
            .cloned()
            .chain(std::iter::once(self.pointer.clone()))
            .collect()
    }

    /// Why the check did not run, when it did not.
    pub(crate) fn why_not(&self) -> Option<&str> {
        self.checked.record.reason.as_deref()
    }
}

/// Everything the sweep learned about one version.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct VersionFacts {
    /// The ref.
    pub reference: String,
    /// Why there is no binary, when there is none.
    pub unbuilt: Option<String>,
    /// What the build's `--help` says it takes.
    pub traits: Option<Traits>,
    /// The headless scans held to the oracle, by fixture id.
    pub oracle: BTreeMap<String, Placed<OracleObservation>>,
    /// The signal checks, by signal name (`term`, `hup`, `quit`, `close`).
    pub signals: BTreeMap<String, Placed<SignalObservation>>,
    /// The kill-and-restart check.
    pub kill_restart: Option<Placed<KillRestartObservation>>,
    /// The idle check.
    pub idle: Option<Placed<IdleObservation>>,
    /// The filter check.
    pub filter: Option<Placed<FilterObservation>>,
    /// The selection-drift check.
    pub drift: Option<Placed<DriftObservation>>,
    /// The descriptor check.
    pub descriptors: Option<Placed<DescriptorObservation>>,
}

/// One paired timing, with where it is in the document.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MeasuredSet {
    /// Its index in the document's `measurements`.
    pub index: usize,
    /// What it measured.
    pub measurement: SweepMeasurement,
    /// Why a version's runs could not be carried out, by ref.
    pub errors: BTreeMap<String, String>,
}
