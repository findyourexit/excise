//! The ownership marker: the file that makes a directory a harness fixture.
//!
//! Every fixture root carries [`MARKER_FILE_NAME`], a regular file (never a symbolic link). The
//! harness refuses to run `excise`, or to mutate anything, in a root without it, so that a
//! mistyped path can never point a deletion test at real data. [`verify_owned`] is that check.
//! [`is_marker_path`] is the one rule for which fixture-relative paths name the marker, which no
//! mutation and no deletion may touch.
//!
//! The generator writes a JSON [`Marker`] as the last thing it does. That makes the marker the
//! fixture's seal too: a directory that is still being generated has none, and a cache entry is
//! only reused when its marker parses and matches the spec it is asked to serve. Fixtures built
//! by other means may put any content in the file; [`verify_owned`] only requires that it is a
//! regular file.

use std::{
    collections::BTreeMap,
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    fixture::{
        GENERATOR_VERSION, MARKER_FILE_NAME, NodeKind,
        caps::{Capabilities, Capability},
        generate::GenerateReport,
        plan::Plan,
        sys::Dir,
    },
    string_enum::string_enum,
};

/// The `schema_version` of the marker document.
pub const MARKER_SCHEMA_VERSION: u32 = 1;

/// The largest marker [`read_marker`] will read.
const MAX_MARKER_BYTES: u64 = 1 << 20;

/// The name the marker is written under before it is renamed into place.
const TEMPORARY_NAME: &str = ".excise-harness-owned.tmp";

string_enum! {
    /// The `document_kind` of a marker.
    pub enum MarkerKind {
        /// The ownership marker of one fixture.
        Marker => "harness-fixture-marker",
    }
}

string_enum! {
    /// What a fixture directory is for.
    pub enum Role {
        /// A cached fixture that runners must not modify.
        Master => "master",
        /// A disposable per-run copy, generated fresh for one run.
        RunCopy => "run-copy",
    }
}

/// The content of the ownership marker: what the generator made and how to recognize it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Marker {
    /// Always `harness-fixture-marker`.
    pub document_kind: MarkerKind,
    /// The version of this document's shape.
    pub schema_version: u32,
    /// The generator version that produced the tree.
    pub generator_version: u32,
    /// Whether this is a cached master or a run copy.
    pub role: Role,
    /// The spec id.
    pub spec_id: String,
    /// The spec hash, seed included.
    pub spec_hash: String,
    /// The seed.
    pub seed: u64,
    /// The manifest hash of the whole plan: identical on every machine for the same spec and seed.
    pub manifest_sha256: String,
    /// The manifest hash of the entries actually created; differs from `manifest_sha256` exactly
    /// where a capability was missing.
    pub realized_sha256: String,
    /// The entries in the plan.
    pub entries: u64,
    /// The entries created.
    pub created: u64,
    /// The entries skipped, counted by the capability they needed.
    pub skipped: BTreeMap<Capability, u64>,
    /// What the file system could do when the fixture was generated.
    pub capabilities: Capabilities,
    /// How long generation took, in milliseconds.
    pub generation_ms: u64,
    /// The worker threads generation used.
    pub threads: u64,
}

impl Marker {
    /// The marker for a fixture generated from `plan`.
    #[must_use]
    pub fn new(
        plan: &Plan,
        role: Role,
        capabilities: &Capabilities,
        report: &GenerateReport,
    ) -> Self {
        Self {
            document_kind: MarkerKind::Marker,
            schema_version: MARKER_SCHEMA_VERSION,
            generator_version: GENERATOR_VERSION,
            role,
            spec_id: plan.spec().id.clone(),
            spec_hash: plan.spec_hash().to_owned(),
            seed: plan.seed(),
            manifest_sha256: plan.manifest_sha256().to_owned(),
            realized_sha256: plan.realized_sha256(capabilities),
            entries: plan.entries().len() as u64,
            created: report.created,
            skipped: report.skipped.clone(),
            capabilities: capabilities.clone(),
            generation_ms: u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
            threads: report.threads as u64,
        }
    }
}

/// A directory that is not a harness fixture.
#[derive(Debug, Error)]
pub enum OwnershipError {
    /// The root cannot be opened as a directory. A root that is a symbolic link is refused too.
    #[error("cannot open the fixture root `{}` (it must be a directory, not a link): {source}", path.display())]
    Root {
        /// The root.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// There is no marker.
    #[error("`{}` has no `{MARKER_FILE_NAME}` marker, so it is not a harness fixture", path.display())]
    Missing {
        /// The root.
        path: PathBuf,
    },
    /// The marker is not a regular file: a directory, or a link that could point anywhere.
    #[error("the `{MARKER_FILE_NAME}` marker of `{}` is a {kind}, not a regular file", path.display())]
    NotRegular {
        /// The root.
        path: PathBuf,
        /// What the marker actually is.
        kind: NodeKind,
    },
    /// The marker could not be inspected.
    #[error("cannot inspect the marker of `{}`: {source}", path.display())]
    Inspect {
        /// The root.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
}

/// A marker that could not be read.
#[derive(Debug, Error)]
pub enum MarkerError {
    /// The directory is not a harness fixture at all.
    #[error(transparent)]
    Ownership(#[from] OwnershipError),
    /// The marker file could not be read.
    #[error("cannot read the marker of `{}`: {source}", path.display())]
    Read {
        /// The root.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The marker is not a harness marker document: another tool's marker, or a damaged one.
    #[error("the marker of `{}` is not a valid harness marker: {source}", path.display())]
    Parse {
        /// The root.
        path: PathBuf,
        /// The underlying error.
        source: serde_json::Error,
    },
}

/// Checks that `root` is a harness fixture: a directory (not a link) holding a marker that is a
/// regular file (not a link). Every runner and every mutator calls this before it acts.
///
/// # Errors
///
/// Returns why the directory is not a fixture.
pub fn verify_owned(root: &Path) -> Result<(), OwnershipError> {
    let directory = Dir::open_root(root).map_err(|source| OwnershipError::Root {
        path: root.to_path_buf(),
        source,
    })?;
    match directory.stat(MARKER_FILE_NAME.as_bytes()) {
        Ok(stat) if stat.kind == NodeKind::File => Ok(()),
        Ok(stat) => Err(OwnershipError::NotRegular {
            path: root.to_path_buf(),
            kind: stat.kind,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(OwnershipError::Missing {
            path: root.to_path_buf(),
        }),
        Err(source) => Err(OwnershipError::Inspect {
            path: root.to_path_buf(),
            source,
        }),
    }
}

/// Whether the fixture-relative `relative` is the ownership marker or lies inside it.
///
/// True when the first `/`-separated component of `relative` is [`MARKER_FILE_NAME`]: the path is
/// `.excise-harness-owned` itself or starts with `.excise-harness-owned/`. The comparison is
/// exact, as in the fixture specs and the mutators, and looks at the first component only:
/// only the root's own entry of that name is the marker, so `sub/.excise-harness-owned`,
/// `.excise-harness-owned-2`, and `.excise-harness-owned.tmp` are other entries.
///
/// The path is taken as it is written. This does not apply the fixture-relative path rule
/// ([`check_fixture_relative_path`](crate::scenario::check_fixture_relative_path)); a caller that
/// needs it checks it separately. [`mutate::apply`](crate::fixture::mutate::apply) refuses every
/// operation on such a path, and scenario validation rejects a `delete` step that would delete
/// one: a root that lost its marker is no fixture any more, and every runner refuses a root like
/// that.
#[must_use]
pub fn is_marker_path(relative: &str) -> bool {
    relative.split('/').next() == Some(MARKER_FILE_NAME)
}

/// Reads the marker of a harness-generated fixture.
///
/// # Errors
///
/// Returns [`MarkerError::Ownership`] when the directory is not a fixture, and a read or parse
/// error when the marker is unreadable or is not a harness marker (fixtures built by other means
/// have a marker without this content).
pub fn read_marker(root: &Path) -> Result<Marker, MarkerError> {
    verify_owned(root)?;
    let read = |source| MarkerError::Read {
        path: root.to_path_buf(),
        source,
    };
    let directory = Dir::open_root(root).map_err(read)?;
    let file = directory
        .open_regular_for_read(MARKER_FILE_NAME.as_bytes())
        .map_err(read)?;
    let mut text = Vec::new();
    file.take(MAX_MARKER_BYTES + 1)
        .read_to_end(&mut text)
        .map_err(read)?;
    if text.len() as u64 > MAX_MARKER_BYTES {
        return Err(read(io::Error::new(
            io::ErrorKind::InvalidData,
            "the marker is larger than 1 MiB",
        )));
    }
    serde_json::from_slice(&text).map_err(|source| MarkerError::Parse {
        path: root.to_path_buf(),
        source,
    })
}

/// Writes the marker into `root`, atomically: the text goes to a temporary name and is renamed
/// into place, so the marker is either absent or complete.
///
/// # Errors
///
/// Returns the I/O or serialization error.
pub fn write_marker(root: &Path, marker: &Marker) -> io::Result<()> {
    let directory = Dir::open_root(root)?;
    let mut text = serde_json::to_string_pretty(marker).map_err(io::Error::other)?;
    text.push('\n');
    let mut file = directory.create_file(TEMPORARY_NAME.as_bytes())?;
    file.write_all(text.as_bytes())?;
    drop(file);
    directory.rename(
        TEMPORARY_NAME.as_bytes(),
        &directory,
        MARKER_FILE_NAME.as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{MARKER_FILE_NAME, is_marker_path};
    use crate::{
        fixture::mutate::{MutateError, apply},
        scenario::MutateOp,
    };

    #[test]
    fn the_marker_and_everything_inside_it_are_marker_paths() {
        let inside = format!("{MARKER_FILE_NAME}/inner");
        let deeper = format!("{MARKER_FILE_NAME}/inner/deeper");
        for path in [MARKER_FILE_NAME, inside.as_str(), deeper.as_str()] {
            assert!(is_marker_path(path), "{path}");
        }
    }

    #[test]
    fn only_the_first_component_can_be_the_marker() {
        // The same name further down is another entry.
        let nested = format!("sub/{MARKER_FILE_NAME}");
        let deeper = format!("a/b/{MARKER_FILE_NAME}/c");
        for path in [nested.as_str(), deeper.as_str()] {
            assert!(!is_marker_path(path), "{path}");
        }
    }

    #[test]
    fn a_name_that_only_resembles_the_marker_is_not_a_marker_path() {
        let lookalikes = [
            String::new(),
            format!("{MARKER_FILE_NAME}-2"),
            format!("{MARKER_FILE_NAME}.tmp"),
            format!("x{MARKER_FILE_NAME}"),
            MARKER_FILE_NAME[1..].to_owned(),
            MARKER_FILE_NAME[..MARKER_FILE_NAME.len() - 1].to_owned(),
        ];
        for lookalike in &lookalikes {
            assert!(!is_marker_path(lookalike), "{lookalike:?}");
        }
    }

    #[test]
    fn the_mutators_refuse_the_paths_this_rule_names_and_no_others() {
        let root = tempfile::tempdir().expect("a temporary directory");
        fs::write(root.path().join(MARKER_FILE_NAME), b"owned").expect("the marker");
        fs::create_dir(root.path().join("sub")).expect("a folder");
        fs::write(root.path().join("sub").join(MARKER_FILE_NAME), b"x").expect("a lookalike");

        let inside = format!("{MARKER_FILE_NAME}/inner");
        for path in [MARKER_FILE_NAME, inside.as_str()] {
            assert!(is_marker_path(path), "{path}");
            assert!(
                matches!(
                    apply(root.path(), MutateOp::Vanish, path),
                    Err(MutateError::Protected { .. })
                ),
                "{path}"
            );
        }
        // The same name in a folder is an ordinary entry, which `vanish` removes like any other.
        let nested = format!("sub/{MARKER_FILE_NAME}");
        assert!(!is_marker_path(&nested));
        apply(root.path(), MutateOp::Vanish, &nested).expect("an ordinary entry goes");
        assert!(root.path().join(MARKER_FILE_NAME).is_file());
    }
}
