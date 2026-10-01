//! Tests of the oracle diff rules, on constructed trees and reports.
//!
//! Every rule of the diff has a tree small enough to check by hand and a report that follows the
//! rule, which must be clean, and then reports that break it, each of which must be caught as
//! exactly the discrepancy the rule defines.

use std::collections::BTreeSet;

use crate::{
    fixture::{
        NodeKind, Oracle, OracleEntry, RelPath,
        oracle::{ORACLE_SCHEMA_VERSION, OracleKind, OraclePlatform},
    },
    headless::document::{
        Bounds, EntryKind, EntryState, FileId, Identity, ReportEntry, ScanDocument, ScanState,
        ScanSummary,
    },
};

use super::{Diff, Discrepancy, DiscrepancyKind, LISTED_PER_KIND, Scan, Unlisted, diff};

const ROOT: &[u8] = b"/scan/root";

// ---------------------------------------------------------------------------------------------
// Trees.

/// A tree as an `lstat` walk would report it, built by hand.
struct Tree {
    entries: Vec<OracleEntry>,
    facts: bool,
}

impl Tree {
    /// A tree of one directory, the root, with metadata of its own that no total may include.
    fn new() -> Self {
        let mut tree = Self {
            entries: Vec::new(),
            facts: true,
        };
        tree.add("", NodeKind::Directory, 64, 4096, 1, 2);
        tree
    }

    /// A tree whose platform has no identity and no allocation facts, as on Windows.
    fn without_facts() -> Self {
        let mut tree = Self::new();
        tree.facts = false;
        for entry in &mut tree.entries {
            entry.allocated = None;
            entry.dev = None;
            entry.ino = None;
            entry.nlink = None;
        }
        tree
    }

    fn add(
        &mut self,
        path: &str,
        kind: NodeKind,
        size: u64,
        allocated: u64,
        inode: u64,
        links: u64,
    ) -> &mut Self {
        let facts = self.facts;
        self.entries.push(OracleEntry {
            path: RelPath::from_bytes(path).expect("a valid path"),
            kind,
            size,
            allocated: facts.then_some(allocated),
            dev: facts.then_some(1),
            ino: facts.then_some(inode),
            nlink: facts.then_some(links),
            mode: facts.then_some(0o755),
            readable: true,
            device_boundary: false,
            symlink_target: None,
            subtree: None,
        });
        self
    }

    fn dir(&mut self, path: &str, inode: u64) -> &mut Self {
        self.add(path, NodeKind::Directory, 64, 4096, inode, 2)
    }

    fn file(&mut self, path: &str, size: u64, allocated: u64, inode: u64, links: u64) -> &mut Self {
        self.add(path, NodeKind::File, size, allocated, inode, links)
    }

    fn link(&mut self, path: &str, target_len: u64, inode: u64) -> &mut Self {
        self.add(path, NodeKind::Symlink, target_len, 0, inode, 1)
    }

    fn unreadable(&mut self, path: &str, inode: u64) -> &mut Self {
        self.dir(path, inode);
        self.entries.last_mut().expect("an entry").readable = false;
        self
    }

    fn boundary(&mut self, path: &str, inode: u64) -> &mut Self {
        self.dir(path, inode);
        let entry = self.entries.last_mut().expect("an entry");
        entry.device_boundary = true;
        entry.dev = self.facts.then_some(2);
        self
    }

    fn oracle(&self) -> Oracle {
        let mut entries = self.entries.clone();
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Oracle {
            document_kind: OracleKind::Oracle,
            schema_version: ORACLE_SCHEMA_VERSION,
            platform: OraclePlatform {
                os: "test".to_owned(),
                identity: self.facts,
                allocation: self.facts,
            },
            entries,
            hard_links: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Reports.

fn absolute(path: &str) -> Vec<u8> {
    let mut bytes = ROOT.to_vec();
    if !path.is_empty() {
        bytes.push(b'/');
        bytes.extend_from_slice(path.as_bytes());
    }
    bytes
}

/// A report entry that is complete, has the identity of the tree entry at `path`, and says
/// nothing of any size: the caller adds what the rule under test needs.
fn entry(path: &str, kind: EntryKind, inode: u64, links: u64) -> ReportEntry {
    ReportEntry {
        path: absolute(path),
        kind,
        state: EntryState::Complete,
        identity: Some(Identity {
            file_id: FileId::Inode { device: 1, inode },
            link_count: Some(links),
            reparse_point: kind == EntryKind::Link,
        }),
        allocated: Bounds {
            lower: 0,
            upper: Some(0),
        },
        reclaimable: Bounds {
            lower: 0,
            upper: Some(0),
        },
        apparent_bytes: 0,
        descendants: 0,
        unscanned_reason: None,
    }
}

trait Sized {
    fn apparent(self, bytes: u128) -> Self;
    fn allocated(self, lower: u128, upper: Option<u128>) -> Self;
    fn reclaimable(self, lower: u128, upper: Option<u128>) -> Self;
    fn below(self, descendants: u64) -> Self;
    fn uncertain(self) -> Self;
}

impl Sized for ReportEntry {
    fn apparent(mut self, bytes: u128) -> Self {
        self.apparent_bytes = bytes;
        self
    }

    fn allocated(mut self, lower: u128, upper: Option<u128>) -> Self {
        self.allocated = Bounds { lower, upper };
        self
    }

    fn reclaimable(mut self, lower: u128, upper: Option<u128>) -> Self {
        self.reclaimable = Bounds { lower, upper };
        self
    }

    fn below(mut self, descendants: u64) -> Self {
        self.descendants = descendants;
        self
    }

    fn uncertain(mut self) -> Self {
        self.state = EntryState::Uncertain;
        self.unscanned_reason = Some("scan coverage is uncertain".to_owned());
        self
    }
}

fn root(links: u64) -> ReportEntry {
    entry("", EntryKind::Root, 1, links)
}

fn dir(path: &str, inode: u64) -> ReportEntry {
    entry(path, EntryKind::Directory, inode, 2)
}

fn file(path: &str, inode: u64, links: u64) -> ReportEntry {
    entry(path, EntryKind::File, inode, links)
}

/// An exact entry: both bounds, allocated and reclaimable, equal.
fn exact(entry: ReportEntry, apparent: u128, allocated: u128, below: u64) -> ReportEntry {
    entry
        .apparent(apparent)
        .allocated(allocated, Some(allocated))
        .reclaimable(allocated, Some(allocated))
        .below(below)
}

fn summary() -> ScanSummary {
    ScanSummary {
        scanned_entries: 0,
        identified_entries: 0,
        unreadable_entries: 0,
        unscanned_entries: 0,
        excluded_entries: 0,
        filesystem_boundaries: 0,
        link_entries: 0,
        deleted_entries: 0,
        deletion_changed_entries: 0,
        deletion_missing_entries: 0,
        deletion_failed_entries: 0,
        deletion_unattempted_entries: 0,
        scan_store_bytes: 0,
        scan_store_limit_bytes: 0,
        last_unreadable_path: None,
        last_unscanned_path: None,
        last_unscanned_reason: None,
        last_worker_error: None,
    }
}

fn document(state: ScanState, entries: Vec<ReportEntry>) -> ScanDocument {
    ScanDocument {
        root: ROOT.to_vec(),
        state,
        summary: summary(),
        entries,
    }
}

/// Holds `document`, from a process that exited with the code of its state, to the tree.
fn run(tree: &Tree, document: &ScanDocument) -> Diff {
    diff(
        &tree.oracle(),
        &Scan {
            root: ROOT,
            document,
            exit_code: Some(document.state.exit_code()),
        },
    )
}

fn kinds(diff: &Diff) -> BTreeSet<DiscrepancyKind> {
    diff.kinds()
}

fn paths(diff: &Diff, kind: DiscrepancyKind) -> BTreeSet<String> {
    diff.discrepancies
        .iter()
        .filter(|discrepancy| discrepancy.kind() == kind)
        .map(|discrepancy| match discrepancy {
            Discrepancy::Missing { path }
            | Discrepancy::Duplicate { path }
            | Discrepancy::OutOfScope { path }
            | Discrepancy::Kind { path, .. }
            | Discrepancy::Identity { path, .. }
            | Discrepancy::ApparentBytes { path, .. }
            | Discrepancy::AllocatedBytes { path, .. }
            | Discrepancy::ReclaimableBytes { path, .. }
            | Discrepancy::Descendants { path, .. }
            | Discrepancy::EntryState { path, .. }
            | Discrepancy::UnscannedReason { path, .. } => {
                if path.is_root() {
                    ".".to_owned()
                } else {
                    path.to_string()
                }
            }
            Discrepancy::Unexpected { path, .. } => path.clone(),
            other => panic!("{other} has no path"),
        })
        .collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

// ---------------------------------------------------------------------------------------------
// A matching report, and directory metadata.

/// root, `a`, and `a/f`: a 100-byte file that occupies one 4 KiB block.
fn plain() -> Tree {
    let mut tree = Tree::new();
    tree.dir("a", 2).file("a/f", 100, 4096, 3, 1);
    tree
}

fn plain_report() -> Vec<ReportEntry> {
    vec![
        exact(root(2), 100, 4096, 2),
        exact(dir("a", 2), 100, 4096, 1),
        exact(file("a/f", 3, 1), 100, 4096, 0),
    ]
}

#[test]
fn a_report_that_follows_every_rule_is_clean() {
    let outcome = run(&plain(), &document(ScanState::Exact, plain_report()));

    assert!(outcome.is_clean(), "{:?}", outcome.discrepancies);
    assert_eq!(outcome.compared.entries, 3);
    assert_eq!(outcome.compared.reported, 3);
    assert_eq!(outcome.compared.identities, 1);
    assert_eq!(outcome.compared.shared_identities, 0);
}

#[test]
fn directory_metadata_is_in_no_total() {
    // The tree's directories have 64 bytes of size and a block of allocation of their own. A report
    // that adds them up counts what the contract excludes.
    let mut entries = plain_report();
    entries[1] = exact(dir("a", 2), 100 + 64, 4096 + 4096, 1);
    entries[0] = exact(root(2), 100 + 128, 4096 + 8192, 2);

    let outcome = run(&plain(), &document(ScanState::Exact, entries));

    assert_eq!(
        kinds(&outcome),
        BTreeSet::from([
            DiscrepancyKind::ApparentBytes,
            DiscrepancyKind::AllocatedBytes,
            DiscrepancyKind::ReclaimableBytes
        ])
    );
    assert_eq!(
        paths(&outcome, DiscrepancyKind::AllocatedBytes),
        set(&[".", "a"])
    );
    assert_eq!(
        paths(&outcome, DiscrepancyKind::ApparentBytes),
        set(&[".", "a"])
    );
}

#[test]
fn the_discrepancy_names_what_the_report_should_have_said() {
    let mut entries = plain_report();
    entries[2] = exact(file("a/f", 3, 1), 100, 8192, 0);

    let outcome = run(&plain(), &document(ScanState::Exact, entries));

    let found = outcome
        .discrepancies
        .iter()
        .find_map(|discrepancy| match discrepancy {
            Discrepancy::AllocatedBytes {
                path,
                expected,
                found,
            } if path.to_string() == "a/f" => Some((*expected, *found)),
            _ => None,
        })
        .expect("a discrepancy at the file");
    assert_eq!(found.0.lower, 4096);
    assert_eq!(found.0.upper, Some(4096));
    assert_eq!(found.1.lower, 8192);
}

// ---------------------------------------------------------------------------------------------
// Allocation once per identity.

/// Two names of one 8 KiB file in one directory.
fn names_in_one_directory() -> Tree {
    let mut tree = Tree::new();
    tree.dir("d", 2)
        .file("d/a", 5000, 8192, 10, 2)
        .file("d/b", 5000, 8192, 10, 2);
    tree
}

#[test]
fn names_in_one_directory_count_once_at_that_directory() {
    let entries = vec![
        exact(root(2), 10_000, 8192, 3),
        exact(dir("d", 2), 10_000, 8192, 2),
        exact(file("d/a", 10, 2), 5000, 0, 0),
        exact(file("d/b", 10, 2), 5000, 0, 0),
    ];

    let outcome = run(
        &names_in_one_directory(),
        &document(ScanState::Exact, entries),
    );

    assert!(outcome.is_clean(), "{:?}", outcome.discrepancies);
    assert_eq!(
        outcome.compared.identities, 1,
        "the file has one, whatever its names"
    );
    assert_eq!(outcome.compared.shared_identities, 1);
}

#[test]
fn a_file_with_two_names_counted_twice_is_caught_at_every_entry_that_counts_it() {
    let entries = vec![
        exact(root(2), 10_000, 16_384, 3),
        exact(dir("d", 2), 10_000, 16_384, 2),
        exact(file("d/a", 10, 2), 5000, 8192, 0),
        exact(file("d/b", 10, 2), 5000, 8192, 0),
    ];

    let outcome = run(
        &names_in_one_directory(),
        &document(ScanState::Exact, entries),
    );

    assert_eq!(
        kinds(&outcome),
        BTreeSet::from([
            DiscrepancyKind::AllocatedBytes,
            DiscrepancyKind::ReclaimableBytes
        ])
    );
    assert_eq!(
        paths(&outcome, DiscrepancyKind::AllocatedBytes),
        set(&[".", "d", "d/a", "d/b"])
    );
}

#[test]
fn file_length_counts_for_every_name() {
    // The allocation counts once; the length of the file counts at each of its names.
    let mut entries = vec![
        exact(root(2), 5000, 8192, 3),
        exact(dir("d", 2), 5000, 8192, 2),
        exact(file("d/a", 10, 2), 5000, 0, 0),
        exact(file("d/b", 10, 2), 5000, 0, 0),
    ];
    entries[0].apparent_bytes = 5000;
    entries[1].apparent_bytes = 5000;

    let outcome = run(
        &names_in_one_directory(),
        &document(ScanState::Exact, entries),
    );

    assert_eq!(
        kinds(&outcome),
        BTreeSet::from([DiscrepancyKind::ApparentBytes])
    );
    assert_eq!(
        paths(&outcome, DiscrepancyKind::ApparentBytes),
        set(&[".", "d"])
    );
}

/// One 4 KiB file with a name in `a` and one in `b`: the lowest entry that holds both is the root.
fn names_in_sibling_directories() -> Tree {
    let mut tree = Tree::new();
    tree.dir("a", 2)
        .dir("b", 3)
        .file("a/x", 10, 4096, 10, 2)
        .file("b/y", 10, 4096, 10, 2);
    tree
}

#[test]
fn names_in_sibling_directories_count_once_at_their_parent_and_nowhere_below_it() {
    let entries = vec![
        exact(root(2), 20, 4096, 4),
        exact(dir("a", 2), 10, 0, 1),
        exact(dir("b", 3), 10, 0, 1),
        exact(file("a/x", 10, 2), 10, 0, 0),
        exact(file("b/y", 10, 2), 10, 0, 0),
    ];

    let outcome = run(
        &names_in_sibling_directories(),
        &document(ScanState::Exact, entries),
    );

    assert!(outcome.is_clean(), "{:?}", outcome.discrepancies);
}

#[test]
fn a_report_that_puts_the_allocation_in_one_sibling_is_caught_in_both() {
    let entries = vec![
        exact(root(2), 20, 4096, 4),
        exact(dir("a", 2), 10, 4096, 1),
        exact(dir("b", 3), 10, 0, 1),
        exact(file("a/x", 10, 2), 10, 4096, 0),
        exact(file("b/y", 10, 2), 10, 0, 0),
    ];

    let outcome = run(
        &names_in_sibling_directories(),
        &document(ScanState::Exact, entries),
    );

    assert_eq!(
        paths(&outcome, DiscrepancyKind::AllocatedBytes),
        set(&["a", "a/x"])
    );
}

#[test]
fn names_at_different_depths_count_at_their_lowest_common_ancestor() {
    let mut tree = Tree::new();
    tree.dir("a", 2)
        .dir("a/sub", 3)
        .file("a/x", 10, 4096, 10, 2)
        .file("a/sub/y", 10, 4096, 10, 2);
    let entries = vec![
        exact(root(2), 20, 4096, 4),
        exact(dir("a", 2), 20, 4096, 3),
        exact(dir("a/sub", 3), 10, 0, 1),
        exact(file("a/x", 10, 2), 10, 0, 0),
        exact(file("a/sub/y", 10, 2), 10, 0, 0),
    ];

    let outcome = run(&tree, &document(ScanState::Exact, entries));

    assert!(outcome.is_clean(), "{:?}", outcome.discrepancies);
}

#[test]
fn names_that_share_only_a_byte_prefix_have_the_root_for_their_ancestor() {
    // `ab/x` and `abc/y` begin with the same two bytes but share no directory.
    let mut tree = Tree::new();
    tree.dir("ab", 2)
        .dir("abc", 3)
        .file("ab/x", 10, 4096, 10, 2)
        .file("abc/y", 10, 4096, 10, 2);
    let entries = vec![
        exact(root(2), 20, 4096, 4),
        exact(dir("ab", 2), 10, 0, 1),
        exact(dir("abc", 3), 10, 0, 1),
        exact(file("ab/x", 10, 2), 10, 0, 0),
        exact(file("abc/y", 10, 2), 10, 0, 0),
    ];

    let outcome = run(&tree, &document(ScanState::Exact, entries));

    assert!(outcome.is_clean(), "{:?}", outcome.discrepancies);
}

// ---------------------------------------------------------------------------------------------
// Reclaimable space.

#[test]
fn space_is_reclaimable_only_where_every_declared_link_was_met() {
    // The file says it has three links and the scan met two names: the third name is somewhere
    // else, so deleting the root cannot be promised to free the space.
    let mut tree = Tree::new();
    tree.dir("a", 2)
        .dir("b", 3)
        .file("a/x", 10, 4096, 10, 3)
        .file("b/y", 10, 4096, 10, 3);
    let held = |entry: ReportEntry| entry.reclaimable(0, Some(4096));
    let entries = vec![
        held(exact(root(2), 20, 4096, 4)),
        exact(dir("a", 2), 10, 0, 1),
        exact(dir("b", 3), 10, 0, 1),
        exact(file("a/x", 10, 3), 10, 0, 0),
        exact(file("b/y", 10, 3), 10, 0, 0),
    ];

    let outcome = run(&tree, &document(ScanState::Exact, entries));

    assert!(outcome.is_clean(), "{:?}", outcome.discrepancies);
}

#[test]
fn claiming_the_space_reclaimable_when_a_link_was_not_met_is_caught() {
    let mut tree = Tree::new();
    tree.dir("a", 2).file("a/x", 10, 4096, 10, 2);
    let entries = vec![
        exact(root(2), 10, 4096, 2),
        exact(dir("a", 2), 10, 4096, 1),
        exact(file("a/x", 10, 2), 10, 4096, 0),
    ];

    let outcome = run(&tree, &document(ScanState::Exact, entries));

    assert_eq!(
        kinds(&outcome),
        BTreeSet::from([DiscrepancyKind::ReclaimableBytes])
    );
    assert_eq!(
        paths(&outcome, DiscrepancyKind::ReclaimableBytes),
        set(&[".", "a", "a/x"])
    );
}

// ---------------------------------------------------------------------------------------------
// Links are not followed.

/// The document of a scan of [`with_a_link`]: exact, and counting the one link it did not follow.
fn link_document(entries: Vec<ReportEntry>) -> ScanDocument {
    let mut document = document(ScanState::Exact, entries);
    document.summary.link_entries = 1;
    document
}

fn with_a_link() -> Tree {
    let mut tree = Tree::new();
    tree.dir("target", 2)
        .file("target/big", 1000, 4096, 3, 1)
        .link("link", 6, 4);
    tree
}

#[test]
fn a_link_is_an_entry_of_its_own_with_nothing_below_it() {
    let mut entries = vec![
        exact(root(2), 1006, 4096, 3),
        exact(dir("target", 2), 1000, 4096, 1),
        exact(file("target/big", 3, 1), 1000, 4096, 0),
        exact(entry("link", EntryKind::Link, 4, 1), 6, 0, 0),
    ];
    entries.sort_by(|left, right| left.path.cmp(&right.path));

    let outcome = run(&with_a_link(), &link_document(entries));

    assert!(outcome.is_clean(), "{:?}", outcome.discrepancies);
}

#[test]
fn a_link_that_was_followed_is_caught() {
    let entries = vec![
        exact(root(2), 2006, 8192, 4),
        exact(dir("target", 2), 1000, 4096, 1),
        exact(file("target/big", 3, 1), 1000, 4096, 0),
        exact(entry("link", EntryKind::Link, 4, 1), 1006, 4096, 1),
    ];

    let outcome = run(&with_a_link(), &link_document(entries));

    assert!(paths(&outcome, DiscrepancyKind::Descendants).contains("link"));
    assert!(paths(&outcome, DiscrepancyKind::ApparentBytes).contains("link"));
    assert!(paths(&outcome, DiscrepancyKind::AllocatedBytes).contains("link"));
}

#[test]
fn a_link_reported_as_a_file_or_a_file_as_a_link_is_a_kind_discrepancy() {
    let entries = vec![
        exact(root(2), 1006, 4096, 3),
        exact(dir("target", 2), 1000, 4096, 1),
        exact(entry("target/big", EntryKind::Link, 3, 1), 1000, 4096, 0),
        exact(file("link", 4, 1), 6, 0, 0),
    ];

    let outcome = run(&with_a_link(), &link_document(entries));

    assert_eq!(
        paths(&outcome, DiscrepancyKind::Kind),
        set(&["link", "target/big"])
    );
}

// ---------------------------------------------------------------------------------------------
// Identity.

#[test]
fn an_identity_that_is_not_the_trees_is_caught() {
    let mut entries = plain_report();
    entries[2] = ReportEntry {
        identity: Some(Identity {
            file_id: FileId::Inode {
                device: 1,
                inode: 99,
            },
            link_count: Some(1),
            reparse_point: false,
        }),
        ..entries[2].clone()
    };
    entries[1].identity = None;

    let outcome = run(&plain(), &document(ScanState::Exact, entries));

    assert_eq!(kinds(&outcome), BTreeSet::from([DiscrepancyKind::Identity]));
    assert_eq!(
        paths(&outcome, DiscrepancyKind::Identity),
        set(&["a", "a/f"])
    );
}

// ---------------------------------------------------------------------------------------------
// Unknown is first class.

/// root, a readable directory with a file, and a directory the scan cannot list.
fn with_an_unreadable_directory() -> Tree {
    let mut tree = Tree::new();
    tree.dir("ok", 2)
        .file("ok/f", 1, 4096, 3, 1)
        .unreadable("locked", 4);
    tree
}

fn unreadable_report() -> Vec<ReportEntry> {
    vec![
        exact(root(2), 1, 4096, 3)
            .allocated(4096, None)
            .reclaimable(4096, None)
            .uncertain(),
        exact(dir("ok", 2), 1, 4096, 1),
        exact(file("ok/f", 3, 1), 1, 4096, 0),
        exact(dir("locked", 4), 0, 0, 0)
            .allocated(0, None)
            .reclaimable(0, None)
            .uncertain(),
    ]
}

fn unreadable_document(entries: Vec<ReportEntry>) -> ScanDocument {
    let mut document = document(ScanState::Uncertain, entries);
    document.summary.unreadable_entries = 1;
    document
}

#[test]
fn a_folder_that_cannot_be_listed_makes_itself_and_everything_above_it_uncertain() {
    let outcome = run(
        &with_an_unreadable_directory(),
        &unreadable_document(unreadable_report()),
    );

    assert!(outcome.is_clean(), "{:?}", outcome.discrepancies);
}

#[test]
fn an_uncertain_entry_may_have_an_exact_upper_bound_or_none() {
    let mut entries = unreadable_report();
    entries[3] = exact(dir("locked", 4), 0, 0, 0).uncertain();
    entries[0] = exact(root(2), 1, 4096, 3).uncertain();

    let outcome = run(
        &with_an_unreadable_directory(),
        &unreadable_document(entries),
    );

    assert!(outcome.is_clean(), "{:?}", outcome.discrepancies);
}

#[test]
fn unreadable_folders_reported_complete_are_caught_with_the_folders_above_them() {
    // The report that says `uncertain` once, for the whole document, and nowhere else.
    let entries = vec![
        exact(root(2), 1, 4096, 3),
        exact(dir("ok", 2), 1, 4096, 1),
        exact(file("ok/f", 3, 1), 1, 4096, 0),
        exact(dir("locked", 4), 0, 0, 0),
    ];

    let outcome = run(
        &with_an_unreadable_directory(),
        &unreadable_document(entries),
    );

    assert_eq!(
        kinds(&outcome),
        BTreeSet::from([DiscrepancyKind::EntryState])
    );
    assert_eq!(
        paths(&outcome, DiscrepancyKind::EntryState),
        set(&[".", "locked"])
    );
}

#[test]
fn the_lower_bound_of_an_uncertain_entry_is_still_exact() {
    let mut entries = unreadable_report();
    entries[0] = exact(root(2), 1, 0, 3)
        .allocated(0, None)
        .reclaimable(4096, None)
        .uncertain();

    let outcome = run(
        &with_an_unreadable_directory(),
        &unreadable_document(entries),
    );

    assert_eq!(
        kinds(&outcome),
        BTreeSet::from([DiscrepancyKind::AllocatedBytes])
    );
}

#[test]
fn an_upper_bound_below_the_lower_bound_is_not_an_acceptable_unknown() {
    let mut entries = unreadable_report();
    entries[0] = exact(root(2), 1, 4096, 3)
        .allocated(4096, Some(100))
        .reclaimable(4096, None)
        .uncertain();

    let outcome = run(
        &with_an_unreadable_directory(),
        &unreadable_document(entries),
    );

    assert_eq!(
        kinds(&outcome),
        BTreeSet::from([DiscrepancyKind::AllocatedBytes])
    );
}

#[test]
fn an_uncertain_entry_has_a_reason_and_no_other_entry_does() {
    let mut entries = unreadable_report();
    entries[3].unscanned_reason = None;
    entries[2].unscanned_reason = Some("because".to_owned());

    let outcome = run(
        &with_an_unreadable_directory(),
        &unreadable_document(entries),
    );

    assert_eq!(
        kinds(&outcome),
        BTreeSet::from([DiscrepancyKind::UnscannedReason])
    );
    assert_eq!(
        paths(&outcome, DiscrepancyKind::UnscannedReason),
        set(&["locked", "ok/f"])
    );
}

// ---------------------------------------------------------------------------------------------
// Scope.

fn with_a_boundary() -> Tree {
    let mut tree = Tree::new();
    tree.boundary("mnt", 2).file("mnt/inner", 10, 4096, 3, 1);
    tree.entries.last_mut().expect("an entry").dev = tree.facts.then_some(2);
    tree
}

fn boundary_document(entries: Vec<ReportEntry>) -> ScanDocument {
    let mut document = document(ScanState::Uncertain, entries);
    document.summary.filesystem_boundaries = 1;
    document
}

fn boundary_report() -> Vec<ReportEntry> {
    let mut mount = exact(dir("mnt", 2), 0, 0, 0).uncertain();
    mount.identity = Some(Identity {
        file_id: FileId::Inode {
            device: 2,
            inode: 2,
        },
        link_count: Some(2),
        reparse_point: false,
    });
    vec![exact(root(2), 0, 0, 1).uncertain(), mount]
}

#[test]
fn a_filesystem_boundary_is_an_uncertain_record_with_nothing_below_it() {
    let outcome = run(&with_a_boundary(), &boundary_document(boundary_report()));

    assert!(outcome.is_clean(), "{:?}", outcome.discrepancies);
    assert_eq!(
        outcome.compared.entries, 2,
        "the volume's file is out of scope"
    );
}

#[test]
fn an_entry_below_a_boundary_means_the_scan_crossed_it() {
    let mut entries = boundary_report();
    entries.push(exact(file("mnt/inner", 3, 1), 10, 4096, 0));

    let outcome = run(&with_a_boundary(), &boundary_document(entries));

    assert!(kinds(&outcome).contains(&DiscrepancyKind::OutOfScope));
    assert_eq!(
        paths(&outcome, DiscrepancyKind::OutOfScope),
        set(&["mnt/inner"])
    );
}

#[test]
fn a_boundary_the_summary_does_not_count_is_caught() {
    let mut document = boundary_document(boundary_report());
    document.summary.filesystem_boundaries = 0;

    let outcome = run(&with_a_boundary(), &document);

    assert_eq!(kinds(&outcome), BTreeSet::from([DiscrepancyKind::Summary]));
}

// ---------------------------------------------------------------------------------------------
// The document: state, exit code, summary.

#[test]
fn the_state_the_tree_calls_for_is_exact_for_a_tree_with_nothing_unknown() {
    let outcome = run(&plain(), &document(ScanState::Uncertain, plain_report()));

    assert_eq!(kinds(&outcome), BTreeSet::from([DiscrepancyKind::State]));
}

#[test]
fn the_exit_code_must_be_the_one_of_the_state_the_report_claims() {
    let document = document(ScanState::Exact, plain_report());
    let tree = plain();
    let oracle = tree.oracle();

    for (code, wrong) in [
        (Some(0), false),
        (Some(2), true),
        (Some(70), true),
        (None, true),
    ] {
        let outcome = diff(
            &oracle,
            &Scan {
                root: ROOT,
                document: &document,
                exit_code: code,
            },
        );
        assert_eq!(!outcome.is_clean(), wrong, "{code:?}");
        if wrong {
            assert_eq!(kinds(&outcome), BTreeSet::from([DiscrepancyKind::ExitCode]));
        }
    }
}

#[test]
fn every_state_has_the_documented_exit_code() {
    assert_eq!(ScanState::Exact.exit_code(), 0);
    assert_eq!(ScanState::Uncertain.exit_code(), 2);
    assert_eq!(ScanState::SummaryOnly.exit_code(), 3);
    assert_eq!(ScanState::Cancelled.exit_code(), 130);
}

#[test]
fn a_report_for_another_root_is_caught() {
    let mut document = document(ScanState::Exact, plain_report());
    document.root = b"/elsewhere".to_vec();

    let outcome = run(&plain(), &document);

    assert!(kinds(&outcome).contains(&DiscrepancyKind::Root));
}

#[test]
fn the_summary_counts_links_unreadable_folders_exclusions_and_deletions() {
    let mut document = document(ScanState::Exact, plain_report());
    document.summary.link_entries = 1;
    document.summary.unreadable_entries = 2;
    document.summary.excluded_entries = 3;
    document.summary.deleted_entries = 4;
    document.summary.deletion_failed_entries = 5;

    let outcome = run(&plain(), &document);

    assert_eq!(kinds(&outcome), BTreeSet::from([DiscrepancyKind::Summary]));
    assert_eq!(outcome.counts[&DiscrepancyKind::Summary], 5);
}

// ---------------------------------------------------------------------------------------------
// Coverage.

#[test]
fn an_entry_the_report_lacks_is_missing_and_changes_no_number_above_it() {
    // The directory's totals are those of the entries the report lists: one missing entry is one
    // discrepancy, not a wrong number on every folder above it.
    let mut tree = plain();
    tree.file("a/g", 50, 4096, 4, 1);

    let outcome = run(&tree, &document(ScanState::Exact, plain_report()));

    assert_eq!(kinds(&outcome), BTreeSet::from([DiscrepancyKind::Missing]));
    assert_eq!(paths(&outcome, DiscrepancyKind::Missing), set(&["a/g"]));
}

#[test]
fn a_path_that_is_not_in_the_tree_is_unexpected() {
    let mut entries = plain_report();
    entries.push(file("a/ghost", 9, 1));
    entries.push(ReportEntry {
        path: b"/somewhere/else".to_vec(),
        ..file("x", 8, 1)
    });
    entries.push(ReportEntry {
        path: absolute("a/../escape"),
        ..file("x", 7, 1)
    });

    let outcome = run(&plain(), &document(ScanState::Exact, entries));

    let why: BTreeSet<String> = outcome
        .discrepancies
        .iter()
        .filter_map(|discrepancy| match discrepancy {
            Discrepancy::Unexpected { why, .. } => Some(why.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        why,
        BTreeSet::from([
            Unlisted::NotInTree.to_string(),
            Unlisted::OutsideRoot.to_string(),
            Unlisted::InvalidPath.to_string()
        ])
    );
}

#[test]
fn an_entry_listed_twice_is_a_duplicate_and_counts_once() {
    let mut entries = plain_report();
    entries.push(entries[2].clone());

    let outcome = run(&plain(), &document(ScanState::Exact, entries));

    assert_eq!(
        kinds(&outcome),
        BTreeSet::from([DiscrepancyKind::Duplicate])
    );
}

#[test]
fn a_report_that_omits_the_root_is_missing_it() {
    let entries = plain_report()[1..].to_vec();

    let outcome = run(&plain(), &document(ScanState::Exact, entries));

    assert!(paths(&outcome, DiscrepancyKind::Missing).contains("."));
}

#[test]
fn only_the_first_discrepancies_of_a_kind_are_listed_and_all_are_counted() {
    let mut tree = Tree::new();
    for index in 0..30_u64 {
        tree.file(&format!("f{index:02}"), 1, 4096, 10 + index, 1);
    }

    let outcome = run(&tree, &document(ScanState::Exact, vec![root(2)]));

    let missing = &outcome.counts[&DiscrepancyKind::Missing];
    assert_eq!(*missing, 30);
    let listed = outcome
        .discrepancies
        .iter()
        .filter(|discrepancy| discrepancy.kind() == DiscrepancyKind::Missing)
        .count();
    assert_eq!(listed as u64, LISTED_PER_KIND);
    assert_eq!(outcome.total(), outcome.counts.values().sum::<u64>());
}

#[test]
fn absorbing_a_diff_adds_its_counts_and_keeps_the_cap_per_kind() {
    let mut first = Diff::default();
    let mut second = Diff::default();
    for _ in 0..(LISTED_PER_KIND + 5) {
        first.record(Discrepancy::Timeout { limit_ms: 1 });
        second.record(Discrepancy::Timeout { limit_ms: 2 });
    }
    second.record(Discrepancy::UnexpectedOutput { stdout_bytes: 3 });

    first.absorb(second);

    assert_eq!(
        first.counts[&DiscrepancyKind::Timeout],
        2 * (LISTED_PER_KIND + 5)
    );
    assert_eq!(first.counts[&DiscrepancyKind::UnexpectedOutput], 1);
    assert_eq!(
        first
            .discrepancies
            .iter()
            .filter(|discrepancy| discrepancy.kind() == DiscrepancyKind::Timeout)
            .count() as u64,
        LISTED_PER_KIND
    );
    assert!(
        first
            .discrepancies
            .iter()
            .any(|discrepancy| discrepancy.kind() == DiscrepancyKind::UnexpectedOutput)
    );
}

// ---------------------------------------------------------------------------------------------
// Where the oracle has no facts.

#[test]
fn without_identity_and_allocation_facts_only_the_rules_that_need_them_are_skipped() {
    let mut tree = Tree::without_facts();
    tree.dir("a", 2).file("a/f", 100, 4096, 3, 1);
    // Every size and identity is wrong, as far as the facts go; the lengths are not.
    let entries = vec![
        root(9).apparent(100).below(2).allocated(7, Some(7)),
        dir("a", 5).apparent(100).below(1),
        file("a/f", 6, 4).apparent(100),
    ];

    let outcome = run(&tree, &document(ScanState::Exact, entries));

    assert!(outcome.is_clean(), "{:?}", outcome.discrepancies);

    let wrong = vec![
        root(9).apparent(100).below(2),
        dir("a", 5).apparent(99).below(1),
        file("a/f", 6, 4).apparent(100),
    ];
    let outcome = run(&tree, &document(ScanState::Exact, wrong));
    assert_eq!(
        kinds(&outcome),
        BTreeSet::from([DiscrepancyKind::ApparentBytes])
    );
}

// ---------------------------------------------------------------------------------------------
// Messages.

#[test]
fn a_message_names_the_root_as_a_dot_and_leaves_the_middle_of_a_long_path_out() {
    use std::fmt::Write as _;

    let mut tree = Tree::new();
    let mut path = String::new();
    for level in 0..12 {
        write!(path, "{level:02}-{}", "n".repeat(60)).expect("a string takes text");
        tree.dir(&path, 2 + level);
        path.push('/');
    }
    let outcome = run(&tree, &document(ScanState::Exact, vec![root(2)]));

    let messages: Vec<String> = outcome
        .discrepancies
        .iter()
        .map(ToString::to_string)
        .collect();

    let long = messages
        .iter()
        .find(|message| message.contains("levels"))
        .expect("a long path is shortened");
    assert!(long.len() < 200, "{long}");
    let state = run(
        &plain(),
        &document(ScanState::Exact, {
            let mut entries = plain_report();
            entries[0].state = EntryState::Uncertain;
            entries
        }),
    );
    assert!(
        state
            .discrepancies
            .iter()
            .any(|discrepancy| discrepancy.to_string().contains("`.` should be")),
        "{:?}",
        state.discrepancies
    );
}

#[test]
fn a_hostile_name_is_escaped_in_a_message() {
    let mut tree = Tree::new();
    tree.file("evil\u{1b}[2Jname", 1, 4096, 3, 1);

    let outcome = run(&tree, &document(ScanState::Exact, vec![root(2)]));

    let message = outcome.discrepancies[0].to_string();
    assert!(!message.contains('\u{1b}'), "{message:?}");
    assert!(message.contains("\\u{1b}"), "{message}");
}

#[test]
fn the_kinds_of_discrepancy_have_the_names_the_expectations_use() {
    for kind in DiscrepancyKind::ALL {
        assert_eq!(
            serde_json::from_value::<DiscrepancyKind>(serde_json::json!(kind.as_str()))
                .expect("a kind"),
            *kind
        );
    }
    assert_eq!(DiscrepancyKind::EntryState.as_str(), "entry-state");
    assert_eq!(DiscrepancyKind::OutOfScope.as_str(), "out-of-scope");
}

#[test]
fn the_kind_of_a_discrepancy_is_the_kind_it_was_built_for() {
    let samples = [
        (
            Discrepancy::Timeout { limit_ms: 5 },
            DiscrepancyKind::Timeout,
        ),
        (
            Discrepancy::NoReport {
                ended: "exited with code 70".to_owned(),
                detail: String::new(),
            },
            DiscrepancyKind::NoReport,
        ),
        (
            Discrepancy::InvalidReport {
                reason: "x".to_owned(),
            },
            DiscrepancyKind::InvalidReport,
        ),
        (
            Discrepancy::Residue {
                files: vec!["home/x".to_owned()],
            },
            DiscrepancyKind::Residue,
        ),
        (
            Discrepancy::FixtureChanged {
                changes: vec!["x".to_owned()],
            },
            DiscrepancyKind::FixtureChanged,
        ),
    ];
    for (discrepancy, kind) in samples {
        assert_eq!(discrepancy.kind(), kind);
        assert!(discrepancy.to_string().starts_with(kind.as_str()));
    }
}
