//! Checking that a cached fixture is still the fixture its marker says it is.
//!
//! A cache entry is trusted only after these checks, cheapest first:
//!
//! 1. **The seal.** The entry has a marker (the last thing generation writes), the marker parses,
//!    and it names this generator version, spec id, spec hash, seed, and manifest hash. Anything
//!    else is a stale, foreign, or unfinished entry.
//! 2. **Consistency.** The realized hash the marker records equals the realized hash recomputed
//!    from the plan and the marker's own capabilities, so the marker agrees with the plan.
//! 3. **The top level.** The entry's top-level names are exactly the planned top-level names plus
//!    the marker. This catches a deleted or added top-level entry for the cost of one directory
//!    listing.
//! 4. **The contents.** Either a *sample* (the [`CHEAP_SAMPLE`] planned entries chosen by a
//!    generator seeded from the manifest hash, each looked up with `lstat` and compared for kind,
//!    size, and link target) or *everything* (a full oracle walk compared with the whole plan).
//!    [`Verify::Auto`] walks everything when the plan has at most [`FULL_VERIFY_LIMIT`] entries,
//!    where it costs milliseconds, and samples otherwise.
//!
//! The sample cannot see a change it does not look at. A fixture that must be exactly right, for
//! example after a runner misbehaved, is checked with [`Verify::Full`].

use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};

use thiserror::Error;

use crate::fixture::{
    GENERATOR_VERSION, MARKER_FILE_NAME, NodeKind,
    caps::Capabilities,
    generate::open_existing,
    marker::{Marker, MarkerError, OwnershipError, Role, read_marker},
    oracle::{Discrepancy, Oracle, OracleError},
    path::RelPath,
    plan::{ManifestEntry, Plan},
    rng::{SplitMix64, derive_seed},
    sys::Dir,
};

/// The number of planned entries the cheap check looks at.
pub const CHEAP_SAMPLE: usize = 64;

/// The largest plan [`Verify::Auto`] checks in full.
pub const FULL_VERIFY_LIMIT: usize = 20_000;

/// How thoroughly to verify a cache entry before reusing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verify {
    /// Everything for small plans, a sample for large ones.
    #[default]
    Auto,
    /// The seal, the top level, and a sample of the contents.
    Cheap,
    /// The seal, the top level, and a full oracle walk compared with the whole plan.
    Full,
}

/// What a successful verification did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    /// Whether the contents were checked in full.
    pub full: bool,
    /// The planned entries compared.
    pub checked: u64,
    /// The planned entries that could not be observed (below unreadable directories).
    pub unverifiable: u64,
    /// How long verification took.
    pub elapsed: Duration,
}

/// Why a cache entry cannot be reused.
#[derive(Debug, Error)]
pub enum IntegrityFailure {
    /// There is no entry.
    #[error("there is no cache entry")]
    Absent,
    /// The entry has no marker: generation never finished, or another tool made the directory.
    #[error("the entry has no ownership marker, so generation never finished")]
    Unsealed,
    /// The marker cannot be read, or is not a harness marker.
    #[error("the marker is unusable: {0}")]
    Marker(String),
    /// The marker describes another fixture than the one asked for.
    #[error("the marker does not match the request: {0}")]
    Mismatch(String),
    /// The top-level names differ from the plan.
    #[error("the top-level entries differ from the plan: expected {expected}, found {found}")]
    TopLevel {
        /// What the plan lists.
        expected: String,
        /// What the directory holds.
        found: String,
    },
    /// Entries differ from the plan.
    #[error("{count} entries differ from the plan, for example {first}")]
    Entries {
        /// How many differences were found.
        count: usize,
        /// The first one.
        first: String,
    },
    /// An I/O error while checking.
    #[error("cannot check the entry: {0}")]
    Io(#[from] io::Error),
    /// The oracle walk failed.
    #[error("the oracle walk failed: {0}")]
    Oracle(#[from] OracleError),
}

/// Verifies the cache entry at `entry` against `plan`.
///
/// # Errors
///
/// Returns why the entry cannot be reused; [`IntegrityFailure::Absent`] means there is nothing
/// to reuse and nothing to remove.
pub fn verify_master(
    entry: &Path,
    plan: &Plan,
    verify: Verify,
) -> Result<(Marker, Verified), IntegrityFailure> {
    let started = Instant::now();
    let marker = match read_marker(entry) {
        Ok(marker) => marker,
        Err(MarkerError::Ownership(OwnershipError::Root { source, .. }))
            if source.kind() == io::ErrorKind::NotFound =>
        {
            return Err(IntegrityFailure::Absent);
        }
        Err(MarkerError::Ownership(OwnershipError::Missing { .. })) => {
            return Err(IntegrityFailure::Unsealed);
        }
        Err(error) => return Err(IntegrityFailure::Marker(error.to_string())),
    };
    check_marker(&marker, plan)?;
    let capabilities = &marker.capabilities;

    let root = Dir::open_root(entry)?;
    check_top_level(&root, plan, capabilities)?;

    let full = match verify {
        Verify::Full => true,
        Verify::Cheap => false,
        Verify::Auto => plan.entries().len() <= FULL_VERIFY_LIMIT,
    };
    let (checked, unverifiable) = if full {
        let comparison = Oracle::collect(entry)?.compare(plan, capabilities);
        if let Some(first) = comparison.discrepancies.first() {
            return Err(IntegrityFailure::Entries {
                count: comparison.discrepancies.len(),
                first: describe(first),
            });
        }
        (comparison.checked, comparison.unverifiable)
    } else {
        (check_sample(&root, plan, capabilities)?, 0)
    };
    Ok((
        marker,
        Verified {
            full,
            checked,
            unverifiable,
            elapsed: started.elapsed(),
        },
    ))
}

fn check_marker(marker: &Marker, plan: &Plan) -> Result<(), IntegrityFailure> {
    let mismatch = |what: &str, found: String, expected: String| {
        Err(IntegrityFailure::Mismatch(format!(
            "{what} is {found}, expected {expected}"
        )))
    };
    if marker.generator_version != GENERATOR_VERSION {
        return mismatch(
            "the generator version",
            marker.generator_version.to_string(),
            GENERATOR_VERSION.to_string(),
        );
    }
    if marker.role != Role::Master {
        return mismatch(
            "the role",
            marker.role.to_string(),
            Role::Master.to_string(),
        );
    }
    if marker.spec_id != plan.spec().id {
        return mismatch(
            "the spec id",
            marker.spec_id.clone(),
            plan.spec().id.clone(),
        );
    }
    if marker.spec_hash != plan.spec_hash() {
        return mismatch(
            "the spec hash",
            marker.spec_hash.clone(),
            plan.spec_hash().to_owned(),
        );
    }
    if marker.seed != plan.seed() {
        return mismatch("the seed", marker.seed.to_string(), plan.seed().to_string());
    }
    if marker.manifest_sha256 != plan.manifest_sha256() {
        return mismatch(
            "the manifest hash",
            marker.manifest_sha256.clone(),
            plan.manifest_sha256().to_owned(),
        );
    }
    let realized = plan.realized_sha256(&marker.capabilities);
    if marker.realized_sha256 != realized {
        return mismatch(
            "the realized hash",
            marker.realized_sha256.clone(),
            realized,
        );
    }
    Ok(())
}

fn check_top_level(
    root: &Dir,
    plan: &Plan,
    capabilities: &Capabilities,
) -> Result<(), IntegrityFailure> {
    let mut expected: Vec<Vec<u8>> = plan
        .realized(capabilities)
        .filter(|entry| entry.path.depth() == 1)
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect();
    expected.push(MARKER_FILE_NAME.as_bytes().to_vec());
    expected.sort();
    let mut found: Vec<Vec<u8>> = root.list()?.into_iter().map(|entry| entry.name).collect();
    found.sort();
    if expected == found {
        return Ok(());
    }
    let show = |names: &[Vec<u8>]| {
        names
            .iter()
            .map(|name| {
                RelPath::from_bytes(name.clone())
                    .map_or_else(|_| "?".to_owned(), |path| path.to_string())
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    Err(IntegrityFailure::TopLevel {
        expected: show(&expected),
        found: show(&found),
    })
}

/// The indices, into the realized entries, that the cheap check looks at: [`CHEAP_SAMPLE`] draws
/// from a generator seeded by the manifest hash, so the sample is the same every time.
pub(crate) fn sample_indices(manifest_sha256: &str, len: usize) -> Vec<usize> {
    let mut stream = SplitMix64::new(derive_seed(0, manifest_sha256.as_bytes()));
    (0..CHEAP_SAMPLE.min(len))
        .map(|_| usize::try_from(stream.below(len as u64)).unwrap_or(0))
        .collect()
}

/// Looks at a deterministic sample of the planned entries and returns how many it compared.
fn check_sample(
    root: &Dir,
    plan: &Plan,
    capabilities: &Capabilities,
) -> Result<u64, IntegrityFailure> {
    let realized: Vec<&ManifestEntry> = plan.realized(capabilities).collect();
    // A directory with a permission override may not be enterable, so nothing below it is
    // sampled.
    let restricted: Vec<&RelPath> = realized
        .iter()
        .filter(|entry| entry.kind == NodeKind::Directory && entry.mode.is_some())
        .map(|entry| &entry.path)
        .collect();
    let mut checked = 0;
    let mut problems = Vec::new();
    for index in sample_indices(plan.manifest_sha256(), realized.len()) {
        let planned = realized[index];
        if restricted
            .iter()
            .any(|directory| planned.path.starts_with(directory) && planned.path != **directory)
        {
            continue;
        }
        checked += 1;
        if let Some(problem) = check_one(root, planned)? {
            problems.push(problem);
        }
    }
    match problems.first() {
        Some(first) => Err(IntegrityFailure::Entries {
            count: problems.len(),
            first: describe(first),
        }),
        None => Ok(checked),
    }
}

fn check_one(root: &Dir, planned: &ManifestEntry) -> io::Result<Option<Discrepancy>> {
    let name = planned.path.file_name().unwrap_or_default();
    let parent = match open_existing(root, planned.path.parent_bytes()) {
        Ok(parent) => parent,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Some(Discrepancy::Missing(planned.path.clone())));
        }
        Err(error) => return Err(error),
    };
    let stat = match parent.stat(name) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Some(Discrepancy::Missing(planned.path.clone())));
        }
        Err(error) => return Err(error),
    };
    if stat.kind != planned.kind {
        return Ok(Some(Discrepancy::Kind {
            path: planned.path.clone(),
            expected: planned.kind,
            found: stat.kind,
        }));
    }
    if stat.kind == NodeKind::File && stat.size != planned.size {
        return Ok(Some(Discrepancy::Size {
            path: planned.path.clone(),
            expected: planned.size,
            found: stat.size,
        }));
    }
    if stat.kind == NodeKind::Symlink {
        let target = parent.read_link(name)?;
        if planned
            .target
            .as_ref()
            .map(crate::fixture::path::LinkTarget::as_bytes)
            != Some(&target[..])
        {
            return Ok(Some(Discrepancy::Target(planned.path.clone())));
        }
    }
    Ok(None)
}

/// A short description of a discrepancy for messages.
pub(crate) fn describe(discrepancy: &Discrepancy) -> String {
    match discrepancy {
        Discrepancy::Missing(path) => format!("`{path}` is missing"),
        Discrepancy::Unexpected(path) => format!("`{path}` is not in the plan"),
        Discrepancy::Kind {
            path,
            expected,
            found,
        } => {
            format!("`{path}` is a {found}, the plan says {expected}")
        }
        Discrepancy::Size {
            path,
            expected,
            found,
        } => {
            format!("`{path}` has {found} bytes, the plan says {expected}")
        }
        Discrepancy::Target(path) => format!("`{path}` links elsewhere than planned"),
        Discrepancy::Mode {
            path,
            expected,
            found,
        } => {
            format!("`{path}` has mode {found:o}, the plan says {expected:o}")
        }
        Discrepancy::LinkGroup(path) => {
            format!("`{path}` is not the file its hard-link group names")
        }
    }
}
