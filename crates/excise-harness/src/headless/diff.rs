//! The oracle diff: checks a scan report against what an independent `lstat` walk found, under
//! Excise's accounting contract (`docs/safety/accounting.md`, `docs/reports.md`).
//!
//! The oracle ([`crate::fixture::Oracle`]) holds raw facts and applies none of Excise's rules.
//! This module is where the rules are applied to those facts, independently of the product. For
//! every entry the report lists, it works out what the report must say and compares:
//!
//! | Rule | What the report must say |
//! |---|---|
//! | Directory metadata is excluded | A directory's own size and blocks are in no total. Its `apparent_bytes` is the sum of the lengths of the files and links below it, and its allocation is the allocation of the identities below it. |
//! | Allocation counts once per identity | Every name of a file shares one `(device, inode)` and one allocation. The allocation counts once, at the lowest entry that holds every name the scan met: the file itself when it has one name, otherwise the lowest directory above all its names. Every name, and every directory below that one, shows 0 for it. File length counts for every name. |
//! | Links are not followed | A symbolic link is an entry of its own: its length is the length of its target text, its allocation is its own blocks, and it has no descendants. |
//! | Reclaimable space | An identity is reclaimable at the entry that holds its allocation when every link the file system declares was met; otherwise its lower bound is 0 and its upper bound is its allocation. |
//! | Unknown is first class | An entry that is, or has below it, a directory that cannot be listed or a filesystem boundary is `uncertain` and carries a reason; its lower bounds are exact and its upper bound is unknown or at least the lower bound. Every other entry is `complete` with exact bounds. |
//! | Scope | A directory on another file system is a boundary: the scan does not cross it, so nothing below it is expected. |
//! | State and exit code | The document is `exact` when no entry is uncertain and `uncertain` otherwise; the exit code is the one that goes with the state the document claims. |
//! | Coverage | Every in-scope oracle entry is in the report exactly once, and the report lists nothing else. |
//!
//! Where the oracle has `null` facts (Windows has neither identity nor allocation), the rules
//! that need them are skipped and the rest are applied.
//!
//! Expectations are computed over the entries the report lists, so a single missing entry is one
//! discrepancy and not a change in the numbers of every directory above it.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
};

use crate::{
    fixture::{NodeKind, Oracle, RelPath, path::write_escaped},
    string_enum::string_enum,
};

use super::document::{
    Bounds, EntryKind, EntryState, FileId, Identity, ReportEntry, ScanDocument, ScanState,
    relative_to,
};

/// How many discrepancies of one kind a [`Diff`] lists. The rest are counted.
pub const LISTED_PER_KIND: u64 = 20;

string_enum! {
    /// The kind of a [`Discrepancy`]: the vocabulary of expected failures
    /// (`expectations/headless.toml`) and of the counts in a run summary.
    pub enum DiscrepancyKind {
        /// The report is for another root than the one that was scanned.
        Root => "root",
        /// The document `state` is not the one the tree calls for.
        State => "state",
        /// The exit code is not the one that goes with the document `state`.
        ExitCode => "exit-code",
        /// A count in the run summary is not the one the tree calls for.
        Summary => "summary",
        /// An entry the scan should have reported is not in the report.
        Missing => "missing",
        /// The report lists a path that is not in the tree, or not below the root.
        Unexpected => "unexpected",
        /// The report lists one path twice.
        Duplicate => "duplicate",
        /// The report lists an entry below a filesystem boundary the scan must not cross.
        OutOfScope => "out-of-scope",
        /// An entry is of another kind than the tree's.
        Kind => "kind",
        /// An entry's identity is not the tree's.
        Identity => "identity",
        /// An entry's `apparent_bytes` is wrong.
        ApparentBytes => "apparent-bytes",
        /// An entry's `allocated_bytes` is wrong.
        AllocatedBytes => "allocated-bytes",
        /// An entry's `reclaimable_bytes` is wrong.
        ReclaimableBytes => "reclaimable-bytes",
        /// An entry's `descendants` is wrong.
        Descendants => "descendants",
        /// An entry's `state` is wrong.
        EntryState => "entry-state",
        /// An entry's `unscanned_reason` does not go with its `state`.
        UnscannedReason => "unscanned-reason",
        /// The scan did not end within its bound and was killed.
        Timeout => "timeout",
        /// The scan ended without a report it could be held to.
        NoReport => "no-report",
        /// The report is not valid under the published schema.
        InvalidReport => "invalid-report",
        /// The scan left files behind in its scratch area.
        Residue => "residue",
        /// The scan wrote to standard output although its report goes to a file.
        UnexpectedOutput => "unexpected-output",
        /// The fixture is not the tree it was before the scan.
        FixtureChanged => "fixture-changed",
    }
}

/// What a bound must be: an exact lower bound, and either an exact upper bound or, where the
/// entry is uncertain, an upper bound that is unknown or at least the lower bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expected {
    /// The lower bound, which is always exact.
    pub lower: u128,
    /// The exact upper bound, or `None` where it may also be unknown.
    pub upper: Option<u128>,
}

impl fmt::Display for Expected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.upper {
            Some(upper) => write!(formatter, "lower {}, upper {upper}", self.lower),
            None => write!(
                formatter,
                "lower {}, upper unknown or at least {}",
                self.lower, self.lower
            ),
        }
    }
}

/// The identity facts of a tree entry: device, inode, and link count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityFacts {
    /// `st_dev`.
    pub device: u64,
    /// `st_ino`.
    pub inode: u64,
    /// `st_nlink`.
    pub links: u64,
    /// Whether the entry is a reparse point, which a report says of every link it does not
    /// follow.
    pub reparse_point: bool,
}

impl fmt::Display for IdentityFacts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "device {}, inode {}, {} links",
            self.device, self.inode, self.links
        )?;
        if self.reparse_point {
            formatter.write_str(", a reparse point")?;
        }
        Ok(())
    }
}

/// Why a reported path has no entry in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unlisted {
    /// The path is not the root or below it.
    OutsideRoot,
    /// The path below the root is not a valid relative path (an empty or `..` component).
    InvalidPath,
    /// The tree has nothing at the path.
    NotInTree,
}

impl fmt::Display for Unlisted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::OutsideRoot => "it is not below the scan root",
            Self::InvalidPath => "it is not a valid path below the scan root",
            Self::NotInTree => "the tree has no such entry",
        })
    }
}

/// One way a scan report, or the run that produced it, breaks the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discrepancy {
    /// The report is for another root.
    Root {
        /// The root that was scanned, escaped for display.
        expected: String,
        /// The root the report names, escaped for display.
        found: String,
    },
    /// The document state is wrong.
    State {
        /// The state the tree calls for.
        expected: ScanState,
        /// The state the report claims.
        found: ScanState,
    },
    /// The exit code does not go with the document state.
    ExitCode {
        /// The state the report claims.
        state: ScanState,
        /// The exit code of that state.
        expected: i32,
        /// The exit code of the process, or `None` when a signal ended it.
        found: Option<i32>,
    },
    /// A summary count is wrong.
    Summary {
        /// The summary field.
        field: &'static str,
        /// The count the tree calls for.
        expected: u64,
        /// The count the report gives.
        found: u64,
    },
    /// An entry the report should list is not in it.
    Missing {
        /// The entry, relative to the root.
        path: RelPath,
    },
    /// The report lists a path that is not in the tree.
    Unexpected {
        /// The path, escaped for display and relative to the root where it is below it.
        path: String,
        /// Why there is no such entry.
        why: Unlisted,
    },
    /// The report lists a path twice.
    Duplicate {
        /// The entry, relative to the root.
        path: RelPath,
    },
    /// The report lists an entry below a filesystem boundary.
    OutOfScope {
        /// The entry, relative to the root.
        path: RelPath,
    },
    /// An entry is of the wrong kind.
    Kind {
        /// The entry, relative to the root.
        path: RelPath,
        /// The kind the tree has.
        expected: EntryKind,
        /// The kind the report gives.
        found: EntryKind,
    },
    /// An entry's identity is wrong.
    Identity {
        /// The entry, relative to the root.
        path: RelPath,
        /// The identity the tree has.
        expected: IdentityFacts,
        /// The identity the report gives, when it gives one.
        found: Option<Identity>,
    },
    /// An entry's file length is wrong.
    ApparentBytes {
        /// The entry, relative to the root.
        path: RelPath,
        /// The length the tree calls for.
        expected: u128,
        /// The length the report gives.
        found: u128,
    },
    /// An entry's allocated space is wrong.
    AllocatedBytes {
        /// The entry, relative to the root.
        path: RelPath,
        /// What the bounds must be.
        expected: Expected,
        /// The bounds the report gives.
        found: Bounds,
    },
    /// An entry's reclaimable space is wrong.
    ReclaimableBytes {
        /// The entry, relative to the root.
        path: RelPath,
        /// What the bounds must be.
        expected: Expected,
        /// The bounds the report gives.
        found: Bounds,
    },
    /// An entry's descendant count is wrong.
    Descendants {
        /// The entry, relative to the root.
        path: RelPath,
        /// The count the tree calls for.
        expected: u64,
        /// The count the report gives.
        found: u64,
    },
    /// An entry's state is wrong.
    EntryState {
        /// The entry, relative to the root.
        path: RelPath,
        /// The state the tree calls for.
        expected: EntryState,
        /// The state the report gives.
        found: EntryState,
    },
    /// An entry's reason does not go with its state: an uncertain entry has one, no other does.
    UnscannedReason {
        /// The entry, relative to the root.
        path: RelPath,
        /// The state the report gives.
        state: EntryState,
        /// The reason the report gives.
        reason: Option<String>,
    },
    /// The scan did not end within its bound.
    Timeout {
        /// The bound, in milliseconds.
        limit_ms: u64,
    },
    /// The scan ended without a report.
    NoReport {
        /// How the process ended, as text.
        ended: String,
        /// The first line of its standard error.
        detail: String,
    },
    /// The report is not valid.
    InvalidReport {
        /// Why.
        reason: String,
    },
    /// The scan left files behind.
    Residue {
        /// The files, relative to the scratch area.
        files: Vec<String>,
    },
    /// The scan wrote to standard output.
    UnexpectedOutput {
        /// How many bytes.
        stdout_bytes: u64,
    },
    /// The fixture changed during the scan.
    FixtureChanged {
        /// The first differences found.
        changes: Vec<String>,
    },
}

impl Discrepancy {
    /// The kind of this discrepancy.
    #[must_use]
    pub const fn kind(&self) -> DiscrepancyKind {
        match self {
            Self::Root { .. } => DiscrepancyKind::Root,
            Self::State { .. } => DiscrepancyKind::State,
            Self::ExitCode { .. } => DiscrepancyKind::ExitCode,
            Self::Summary { .. } => DiscrepancyKind::Summary,
            Self::Missing { .. } => DiscrepancyKind::Missing,
            Self::Unexpected { .. } => DiscrepancyKind::Unexpected,
            Self::Duplicate { .. } => DiscrepancyKind::Duplicate,
            Self::OutOfScope { .. } => DiscrepancyKind::OutOfScope,
            Self::Kind { .. } => DiscrepancyKind::Kind,
            Self::Identity { .. } => DiscrepancyKind::Identity,
            Self::ApparentBytes { .. } => DiscrepancyKind::ApparentBytes,
            Self::AllocatedBytes { .. } => DiscrepancyKind::AllocatedBytes,
            Self::ReclaimableBytes { .. } => DiscrepancyKind::ReclaimableBytes,
            Self::Descendants { .. } => DiscrepancyKind::Descendants,
            Self::EntryState { .. } => DiscrepancyKind::EntryState,
            Self::UnscannedReason { .. } => DiscrepancyKind::UnscannedReason,
            Self::Timeout { .. } => DiscrepancyKind::Timeout,
            Self::NoReport { .. } => DiscrepancyKind::NoReport,
            Self::InvalidReport { .. } => DiscrepancyKind::InvalidReport,
            Self::Residue { .. } => DiscrepancyKind::Residue,
            Self::UnexpectedOutput { .. } => DiscrepancyKind::UnexpectedOutput,
            Self::FixtureChanged { .. } => DiscrepancyKind::FixtureChanged,
        }
    }
}

impl fmt::Display for Discrepancy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: ", self.kind())?;
        match self {
            Self::Root { .. }
            | Self::State { .. }
            | Self::ExitCode { .. }
            | Self::Summary { .. } => self.fmt_document(formatter),
            Self::Missing { .. }
            | Self::Unexpected { .. }
            | Self::Duplicate { .. }
            | Self::OutOfScope { .. } => self.fmt_coverage(formatter),
            Self::Timeout { .. }
            | Self::NoReport { .. }
            | Self::InvalidReport { .. }
            | Self::Residue { .. }
            | Self::UnexpectedOutput { .. }
            | Self::FixtureChanged { .. } => self.fmt_run(formatter),
            _ => self.fmt_entry(formatter),
        }
    }
}

impl Discrepancy {
    fn fmt_document(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root { expected, found } => write!(
                formatter,
                "the report is for `{found}`, the scan was of `{expected}`"
            ),
            Self::State { expected, found } => {
                write!(
                    formatter,
                    "the tree is {expected} but the report says `{found}`"
                )
            }
            Self::ExitCode {
                state,
                expected,
                found: Some(found),
            } => write!(
                formatter,
                "the report is `{state}`, which goes with exit code {expected}, but the process exited {found}"
            ),
            Self::ExitCode {
                state, expected, ..
            } => write!(
                formatter,
                "the report is `{state}`, which goes with exit code {expected}, but a signal ended the process"
            ),
            Self::Summary {
                field,
                expected,
                found,
            } => write!(
                formatter,
                "`summary.{field}` is {found}, the tree calls for {expected}"
            ),
            _ => Ok(()),
        }
    }

    fn fmt_coverage(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { path } => write!(
                formatter,
                "`{}` is in the tree but not in the report",
                At(path)
            ),
            Self::Unexpected { path, why } => {
                write!(formatter, "the report lists `{}`, but {why}", elide(path))
            }
            Self::Duplicate { path } => {
                write!(formatter, "`{}` is listed more than once", At(path))
            }
            Self::OutOfScope { path } => write!(
                formatter,
                "`{}` is below a filesystem boundary that the scan must not cross",
                At(path)
            ),
            _ => Ok(()),
        }
    }

    fn fmt_entry(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kind {
                path,
                expected,
                found,
            } => write!(
                formatter,
                "`{}` is a {expected}, the report says {found}",
                At(path)
            ),
            Self::Identity {
                path,
                expected,
                found: Some(found),
            } => write!(
                formatter,
                "`{}` has {expected}; the report says {}",
                At(path),
                describe_identity(found)
            ),
            Self::Identity { path, expected, .. } => write!(
                formatter,
                "`{}` has {expected}; the report gives no identity",
                At(path)
            ),
            Self::ApparentBytes {
                path,
                expected,
                found,
            } => write!(
                formatter,
                "`{}` should be {expected}, the report says {found}",
                At(path)
            ),
            Self::AllocatedBytes {
                path,
                expected,
                found,
            }
            | Self::ReclaimableBytes {
                path,
                expected,
                found,
            } => write!(
                formatter,
                "`{}` should be [{expected}], the report says [{found}]",
                At(path)
            ),
            Self::Descendants {
                path,
                expected,
                found,
            } => write!(
                formatter,
                "`{}` has {expected} descendants, the report says {found}",
                At(path)
            ),
            Self::EntryState {
                path,
                expected,
                found,
            } => write!(
                formatter,
                "`{}` should be `{expected}`, the report says `{found}`",
                At(path)
            ),
            Self::UnscannedReason {
                path,
                state,
                reason: Some(reason),
            } => write!(
                formatter,
                "`{}` is `{state}` but has the reason \"{reason}\"",
                At(path)
            ),
            Self::UnscannedReason { path, state, .. } => {
                write!(formatter, "`{}` is `{state}` and has no reason", At(path))
            }
            _ => Ok(()),
        }
    }

    fn fmt_run(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout { limit_ms } => write!(
                formatter,
                "the scan did not end within {limit_ms} ms and was killed"
            ),
            Self::NoReport { ended, detail } if detail.is_empty() => {
                write!(formatter, "the scan {ended}")
            }
            Self::NoReport { ended, detail } => write!(formatter, "the scan {ended} ({detail})"),
            Self::InvalidReport { reason } => formatter.write_str(reason),
            Self::Residue { files } => {
                write!(formatter, "the scan left {} behind", files.join(", "))
            }
            Self::UnexpectedOutput { stdout_bytes } => write!(
                formatter,
                "the scan wrote {stdout_bytes} bytes to standard output"
            ),
            Self::FixtureChanged { changes } => write!(
                formatter,
                "the fixture changed during the scan: {}",
                changes.join("; ")
            ),
            _ => Ok(()),
        }
    }
}

fn describe_identity(identity: &Identity) -> String {
    let links = identity.link_count.map_or_else(
        || "an unknown number of links".to_owned(),
        |links| format!("{links} links"),
    );
    let mut text = match identity.file_id {
        FileId::Inode { device, inode } => format!("device {device}, inode {inode}, {links}"),
        FileId::Lowres { volume, index } => format!("volume {volume}, index {index}, {links}"),
        FileId::Highres { volume, file } => format!("volume {volume}, file {file}, {links}"),
    };
    if identity.reparse_point {
        text.push_str(", a reparse point");
    }
    text
}

/// An entry's path as a message shows it: the root as `.`, and a path too long to read with its
/// long names and its middle left out.
struct At<'a>(&'a RelPath);

impl fmt::Display for At<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_root() {
            formatter.write_str(".")
        } else {
            formatter.write_str(&elide(&self.0.to_string()))
        }
    }
}

/// `path`, a `/`-separated path as text, with every component of more than 20 characters cut to
/// its first 12 and `..`, and a path of more than 8 components cut to its first 3 and last 4, with
/// the number of levels in between.
fn elide(path: &str) -> String {
    const LONGEST_NAME: usize = 20;
    const KEPT_OF_NAME: usize = 12;
    const MOST_LEVELS: usize = 8;
    let shorten = |name: &str| -> String {
        if name.chars().count() > LONGEST_NAME {
            let head: String = name.chars().take(KEPT_OF_NAME).collect();
            format!("{head}..")
        } else {
            name.to_owned()
        }
    };
    let names: Vec<String> = path.split('/').map(shorten).collect();
    if names.len() <= MOST_LEVELS {
        return names.join("/");
    }
    let hidden = names.len() - 7;
    format!(
        "{}/..({hidden} levels)../{}",
        names[..3].join("/"),
        names[names.len() - 4..].join("/")
    )
}

/// What was compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Compared {
    /// The entries of the tree inside the scan scope, the root included.
    pub entries: u64,
    /// The entries the report lists.
    pub reported: u64,
    /// The file identities the scan met, counted once each.
    pub identities: u64,
    /// The identities with more than one name.
    pub shared_identities: u64,
}

/// The result of a diff: how many discrepancies of each kind there are, and the first of each.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Diff {
    /// The first [`LISTED_PER_KIND`] discrepancies of each kind, in the order found: the document
    /// first, then coverage, then the entries in canonical path order.
    pub discrepancies: Vec<Discrepancy>,
    /// How many discrepancies of each kind there are, listed or not.
    pub counts: BTreeMap<DiscrepancyKind, u64>,
    /// What was compared.
    pub compared: Compared,
}

impl Diff {
    /// Whether the report matches the tree everywhere.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.counts.is_empty()
    }

    /// How many discrepancies there are, listed or not.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.counts.values().sum()
    }

    /// The kinds of discrepancy there are.
    #[must_use]
    pub fn kinds(&self) -> BTreeSet<DiscrepancyKind> {
        self.counts.keys().copied().collect()
    }

    /// Records a discrepancy: counted always, listed while its kind has room.
    pub fn record(&mut self, discrepancy: Discrepancy) {
        let count = self.counts.entry(discrepancy.kind()).or_insert(0);
        *count += 1;
        if *count <= LISTED_PER_KIND {
            self.discrepancies.push(discrepancy);
        }
    }

    /// Adds the discrepancies of `other` to this diff: all of its counts, and as many of its
    /// listed discrepancies as each kind has room for. What `other` compared replaces what this
    /// diff compared, unless `other` compared nothing.
    pub fn absorb(&mut self, other: Self) {
        for (kind, count) in other.counts {
            *self.counts.entry(kind).or_insert(0) += count;
        }
        for discrepancy in other.discrepancies {
            let kind = discrepancy.kind();
            let listed = self
                .discrepancies
                .iter()
                .filter(|listed| listed.kind() == kind)
                .count();
            if (listed as u64) < LISTED_PER_KIND {
                self.discrepancies.push(discrepancy);
            }
        }
        if other.compared != Compared::default() {
            self.compared = other.compared;
        }
    }
}

/// A scan, as the oracle diff sees it.
#[derive(Debug, Clone, Copy)]
pub struct Scan<'a> {
    /// The root the scan was asked to read, as the bytes a report writes it in
    /// ([`path_bytes`](super::document::path_bytes)).
    pub root: &'a [u8],
    /// The report the scan wrote.
    pub document: &'a ScanDocument,
    /// The exit code of the process, or `None` when a signal ended it.
    pub exit_code: Option<i32>,
}

/// Checks `scan` against `oracle`. See the module documentation for the rules.
#[must_use]
pub fn diff(oracle: &Oracle, scan: &Scan<'_>) -> Diff {
    let mut out = Diff::default();
    let scope = Scope::of(oracle);
    document_level(scan, &scope, &mut out);
    let reported = join(oracle, scan, &scope, &mut out);
    let observed: Vec<bool> = (0..oracle.entries.len())
        .map(|index| scope.covered[index] && reported[index].is_some())
        .collect();
    for (index, entry) in oracle.entries.iter().enumerate() {
        if scope.covered[index] && reported[index].is_none() {
            out.record(Discrepancy::Missing {
                path: entry.path.clone(),
            });
        }
    }
    let model = Model::build(oracle, &observed);
    out.compared = Compared {
        entries: count(scope.covered.iter().filter(|inside| **inside).count()),
        reported: count(scan.document.entries.len()),
        identities: model.identities,
        shared_identities: model.shared_identities,
    };
    for index in 0..oracle.entries.len() {
        if let (true, Some(reported)) = (observed[index], reported[index]) {
            check_entry(
                oracle,
                index,
                &scan.document.entries[reported],
                &model,
                &mut out,
            );
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Scope.

/// Which entries of the tree the scan is expected to cover, and what it is expected to count.
struct Scope {
    /// Entries inside the scan scope: not below a filesystem boundary.
    covered: Vec<bool>,
    /// Directories inside the scope that cannot be listed.
    unreadable: u64,
    /// Filesystem boundaries inside the scope.
    boundaries: u64,
    /// Symbolic links inside the scope.
    links: u64,
}

impl Scope {
    fn of(oracle: &Oracle) -> Self {
        let mut scope = Self {
            covered: vec![true; oracle.entries.len()],
            unreadable: 0,
            boundaries: 0,
            links: 0,
        };
        // Entries are in canonical order, so everything below a directory follows it directly.
        let mut pruned: Option<&RelPath> = None;
        for (index, entry) in oracle.entries.iter().enumerate() {
            if let Some(boundary) = pruned {
                if entry.path.starts_with(boundary) {
                    scope.covered[index] = false;
                    continue;
                }
                pruned = None;
            }
            match entry.kind {
                NodeKind::Directory if entry.device_boundary => {
                    scope.boundaries += 1;
                    pruned = Some(&entry.path);
                }
                NodeKind::Directory if !entry.readable => scope.unreadable += 1,
                NodeKind::Symlink => scope.links += 1,
                _ => {}
            }
        }
        scope
    }

    /// The state a scan of this tree must report.
    const fn expected_state(&self) -> ScanState {
        if self.unreadable + self.boundaries > 0 {
            ScanState::Uncertain
        } else {
            ScanState::Exact
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The document.

fn document_level(scan: &Scan<'_>, scope: &Scope, out: &mut Diff) {
    let document = scan.document;
    if document.root != scan.root {
        out.record(Discrepancy::Root {
            expected: escape(scan.root),
            found: escape(&document.root),
        });
    }
    let expected = scope.expected_state();
    if document.state != expected {
        out.record(Discrepancy::State {
            expected,
            found: document.state,
        });
    }
    let code = document.state.exit_code();
    if scan.exit_code != Some(code) {
        out.record(Discrepancy::ExitCode {
            state: document.state,
            expected: code,
            found: scan.exit_code,
        });
    }
    let summary = &document.summary;
    for (field, expected, found) in [
        (
            "unreadable_entries",
            scope.unreadable,
            summary.unreadable_entries,
        ),
        ("link_entries", scope.links, summary.link_entries),
        (
            "filesystem_boundaries",
            scope.boundaries,
            summary.filesystem_boundaries,
        ),
        ("excluded_entries", 0, summary.excluded_entries),
        ("deleted_entries", 0, summary.deleted_entries),
        (
            "deletion_changed_entries",
            0,
            summary.deletion_changed_entries,
        ),
        (
            "deletion_missing_entries",
            0,
            summary.deletion_missing_entries,
        ),
        (
            "deletion_failed_entries",
            0,
            summary.deletion_failed_entries,
        ),
        (
            "deletion_unattempted_entries",
            0,
            summary.deletion_unattempted_entries,
        ),
    ] {
        if expected != found {
            out.record(Discrepancy::Summary {
                field,
                expected,
                found,
            });
        }
    }
}

fn escape(bytes: &[u8]) -> String {
    let mut text = String::new();
    // Writing into a `String` cannot fail.
    let _ = write_escaped(&mut text, bytes);
    text
}

// ---------------------------------------------------------------------------------------------
// Joining the report to the tree.

/// Relates every report entry to the oracle entry at its path. Returns, for each oracle entry,
/// the index of the report entry that names it. Paths that name nothing, and entries named twice
/// or outside the scope, are recorded as discrepancies.
fn join(oracle: &Oracle, scan: &Scan<'_>, scope: &Scope, out: &mut Diff) -> Vec<Option<usize>> {
    let mut reported: Vec<Option<usize>> = vec![None; oracle.entries.len()];
    for (position, entry) in scan.document.entries.iter().enumerate() {
        let Some(relative) = relative_to(scan.root, &entry.path) else {
            out.record(Discrepancy::Unexpected {
                path: escape(&entry.path),
                why: Unlisted::OutsideRoot,
            });
            continue;
        };
        let Ok(path) = RelPath::from_bytes(relative) else {
            out.record(Discrepancy::Unexpected {
                path: escape(relative),
                why: Unlisted::InvalidPath,
            });
            continue;
        };
        let Some(index) = index_of(oracle, &path) else {
            out.record(Discrepancy::Unexpected {
                path: escape(relative),
                why: Unlisted::NotInTree,
            });
            continue;
        };
        if !scope.covered[index] {
            out.record(Discrepancy::OutOfScope { path });
        } else if reported[index].replace(position).is_some() {
            out.record(Discrepancy::Duplicate { path });
        }
    }
    reported
}

fn index_of(oracle: &Oracle, path: &RelPath) -> Option<usize> {
    oracle
        .entries
        .binary_search_by(|entry| entry.path.cmp(path))
        .ok()
}

// ---------------------------------------------------------------------------------------------
// The model: what every entry must say.

/// What one entry, and everything below it, adds up to.
#[derive(Debug, Clone, Copy, Default)]
struct Totals {
    /// File length of the non-directories at and below the entry, every name counted.
    apparent: u128,
    /// Allocation of the identities held at or below the entry.
    allocated: u128,
    /// The lower bound of the space that deleting the entry reclaims.
    reclaimable: u128,
    /// Entries below the entry that the report lists.
    descendants: u64,
    /// Whether the entry is, or has below it, a directory that cannot be listed or a boundary.
    uncertain: bool,
}

/// One file identity, as the scan meets it: the names it has in the report.
struct Group {
    /// The first name, as an index into the oracle.
    first: usize,
    /// How many names the scan meets.
    names: u64,
    /// The component-wise lowest common ancestor of the names: the entry that holds the
    /// allocation.
    holder: Vec<u8>,
    /// The allocation, in bytes.
    allocated: u128,
    /// The link count the file system declares, when every name agrees.
    declared: Option<u64>,
}

struct Model {
    /// Per oracle entry.
    totals: Vec<Totals>,
    /// Whether identity and allocation facts exist, so that allocation can be checked.
    accounting: bool,
    identities: u64,
    shared_identities: u64,
}

impl Model {
    fn build(oracle: &Oracle, observed: &[bool]) -> Self {
        let count = oracle.entries.len();
        let accounting = oracle.platform.identity && oracle.platform.allocation;
        let mut totals = vec![Totals::default(); count];
        let mut groups: HashMap<(u64, u64), Group> = HashMap::new();
        let mut identities = 0_u64;
        for (index, entry) in oracle.entries.iter().enumerate() {
            if !observed[index] {
                continue;
            }
            if entry.kind == NodeKind::Directory {
                totals[index].uncertain = entry.device_boundary || !entry.readable;
                continue;
            }
            totals[index].apparent = u128::from(entry.size);
            if !accounting {
                continue;
            }
            let (Some(device), Some(inode)) = (entry.dev, entry.ino) else {
                continue;
            };
            let allocated = u128::from(entry.allocated.unwrap_or(0));
            groups
                .entry((device, inode))
                .and_modify(|group| {
                    group.names += 1;
                    group.holder = common_ancestor(&group.holder, entry.path.as_bytes());
                    if group.declared != entry.nlink {
                        group.declared = None;
                    }
                })
                .or_insert_with(|| {
                    identities += 1;
                    Group {
                        first: index,
                        names: 1,
                        holder: entry.path.as_bytes().to_vec(),
                        allocated,
                        declared: entry.nlink,
                    }
                });
        }
        let mut shared_identities = 0_u64;
        for group in groups.values() {
            let holder = if group.names == 1 {
                Some(group.first)
            } else {
                shared_identities += 1;
                RelPath::from_bytes(group.holder.clone())
                    .ok()
                    .and_then(|path| index_of(oracle, &path))
            };
            let Some(holder) = holder else { continue };
            totals[holder].allocated += group.allocated;
            if group
                .declared
                .is_some_and(|declared| group.names >= declared)
            {
                totals[holder].reclaimable += group.allocated;
            }
        }

        // Entries come in canonical order, so a parent precedes its children and one pass from
        // the end adds every entry to its parent after everything below it is in.
        let parents = parents(oracle);
        for index in (1..count).rev() {
            let child = totals[index];
            let parent = &mut totals[parents[index]];
            parent.apparent += child.apparent;
            parent.allocated += child.allocated;
            parent.reclaimable += child.reclaimable;
            parent.uncertain |= child.uncertain;
            parent.descendants += child.descendants + u64::from(observed[index]);
        }
        Self {
            totals,
            accounting,
            identities,
            shared_identities,
        }
    }
}

/// The index of every entry's parent. The root, and anything without a parent, is its own.
fn parents(oracle: &Oracle) -> Vec<usize> {
    let mut parents = vec![0_usize; oracle.entries.len()];
    let mut open: Vec<usize> = vec![0];
    for (index, entry) in oracle.entries.iter().enumerate().skip(1) {
        while let Some(&top) = open.last() {
            if entry.path.starts_with(&oracle.entries[top].path) {
                break;
            }
            open.pop();
        }
        parents[index] = open.last().copied().unwrap_or(0);
        if entry.kind == NodeKind::Directory {
            open.push(index);
        }
    }
    parents
}

/// The longest shared prefix of two `/`-separated paths that ends at a component boundary.
fn common_ancestor(left: &[u8], right: &[u8]) -> Vec<u8> {
    let mut shared: Vec<u8> = Vec::new();
    let mut first = true;
    for (a, b) in left
        .split(|byte| *byte == b'/')
        .zip(right.split(|byte| *byte == b'/'))
    {
        if a != b {
            break;
        }
        if !first {
            shared.push(b'/');
        }
        first = false;
        shared.extend_from_slice(a);
    }
    shared
}

// ---------------------------------------------------------------------------------------------
// One entry.

fn check_entry(
    oracle: &Oracle,
    index: usize,
    reported: &ReportEntry,
    model: &Model,
    out: &mut Diff,
) {
    let entry = &oracle.entries[index];
    let totals = &model.totals[index];
    let path = || entry.path.clone();

    let kind = match entry.kind {
        _ if index == 0 => EntryKind::Root,
        NodeKind::Directory => EntryKind::Directory,
        NodeKind::File | NodeKind::Other => EntryKind::File,
        NodeKind::Symlink => EntryKind::Link,
    };
    if reported.kind != kind {
        out.record(Discrepancy::Kind {
            path: path(),
            expected: kind,
            found: reported.kind,
        });
    }

    if let (Some(device), Some(inode), Some(links)) = (entry.dev, entry.ino, entry.nlink) {
        let facts = IdentityFacts {
            device,
            inode,
            links,
            reparse_point: entry.kind == NodeKind::Symlink,
        };
        let matches = reported.identity.is_some_and(|identity| {
            identity.file_id == FileId::Inode { device, inode }
                && identity.link_count == Some(links)
                && identity.reparse_point == facts.reparse_point
        });
        if !matches {
            out.record(Discrepancy::Identity {
                path: path(),
                expected: facts,
                found: reported.identity,
            });
        }
    }

    if reported.apparent_bytes != totals.apparent {
        out.record(Discrepancy::ApparentBytes {
            path: path(),
            expected: totals.apparent,
            found: reported.apparent_bytes,
        });
    }
    if reported.descendants != totals.descendants {
        out.record(Discrepancy::Descendants {
            path: path(),
            expected: totals.descendants,
            found: reported.descendants,
        });
    }

    if model.accounting {
        if let Some(expected) = mismatch(
            reported.allocated,
            totals.allocated,
            totals.allocated,
            totals.uncertain,
        ) {
            out.record(Discrepancy::AllocatedBytes {
                path: path(),
                expected,
                found: reported.allocated,
            });
        }
        if let Some(expected) = mismatch(
            reported.reclaimable,
            totals.reclaimable,
            totals.allocated,
            totals.uncertain,
        ) {
            out.record(Discrepancy::ReclaimableBytes {
                path: path(),
                expected,
                found: reported.reclaimable,
            });
        }
    }

    let expected_state = if totals.uncertain {
        EntryState::Uncertain
    } else {
        EntryState::Complete
    };
    if reported.state != expected_state {
        out.record(Discrepancy::EntryState {
            path: path(),
            expected: expected_state,
            found: reported.state,
        });
    }
    if reported.unscanned_reason.is_some() != (reported.state == EntryState::Uncertain) {
        out.record(Discrepancy::UnscannedReason {
            path: path(),
            state: reported.state,
            reason: reported.unscanned_reason.clone(),
        });
    }
}

/// What the bounds of an entry must be, when `found` is not it.
///
/// The lower bound is always exact. The upper bound is exact too, except where the entry is
/// uncertain: there it may be unknown, or anything at least the lower bound.
fn mismatch(found: Bounds, lower: u128, upper: u128, uncertain: bool) -> Option<Expected> {
    let acceptable = found.lower == lower
        && if uncertain {
            found.upper.is_none_or(|bound| bound >= lower)
        } else {
            found.upper == Some(upper)
        };
    (!acceptable).then_some(Expected {
        lower,
        upper: if uncertain { None } else { Some(upper) },
    })
}

/// A count of entries, saturating where `usize` is wider than `u64`.
fn count(entries: usize) -> u64 {
    u64::try_from(entries).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
