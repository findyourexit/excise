//! A bounded account of the entries one folder holds, to tell whether two readings of it name the
//! same ones.
//!
//! A deletion's overlay records the folder that held the removed entry as the file system has it
//! now, and the folder's modification time is what later ties a deletion of that folder to the
//! map. The time describes every change to the folder's entries, not only the removal, so the
//! overlay may record it only when the entries the folder holds are the ones the overlay
//! publishes for it. Comparing the file system's listing with the map's by name, kind, and
//! identity must not hold either of them: a folder can hold millions of entries, and the memory
//! a comparison uses does not grow with the folder. A [`FolderDigest`] is what each side
//! accumulates instead.

use std::ffi::OsStr;

use file_id::FileId;
use sha2::{Digest as _, Sha256};

use super::path_reducer::PathEntryKind;
use crate::file_id_codec::append_file_id;

/// The entries of one folder, as a count and a 256-bit sum.
///
/// Each entry contributes a term: the SHA-256 of its name, its kind, and the identity of the
/// file system object that has the name. The terms are added lane by lane, modulo 2^64, so the
/// sum does not depend on the order the entries arrive in, and a listing in the file system's
/// order equals the same entries in the map's. Two folders have the same digest only when they
/// hold the same entries, up to a collision of a 256-bit hash, which nobody who can make files
/// in a folder can steer: the identity of a file is the file system's to give.
#[derive(Clone, Debug, Default)]
pub(crate) struct FolderDigest {
    entries: u64,
    sum: [u64; 4],
    /// The identity's encoding, kept between entries so that adding one allocates nothing.
    scratch: Vec<u8>,
}

impl FolderDigest {
    /// Adds the entry `name` of the folder. `identity` is the file system's identity for it as
    /// the scan recorded it, or `None` for an entry the scan recorded none for: that entry equals
    /// none read from the file system, which always has one.
    pub(crate) fn add(&mut self, name: &OsStr, kind: PathEntryKind, identity: Option<&FileId>) {
        let name = name.as_encoded_bytes();
        let mut term = Sha256::new();
        term.update(u64::try_from(name.len()).unwrap_or(u64::MAX).to_le_bytes());
        term.update(name);
        term.update([kind_code(kind)]);
        match identity {
            Some(identity) => {
                self.scratch.clear();
                append_file_id(identity, &mut self.scratch);
                term.update([1]);
                term.update(&self.scratch);
            }
            None => term.update([0]),
        }
        let term = term.finalize();
        let (terms, _) = term.as_chunks::<8>();
        for (lane, bytes) in self.sum.iter_mut().zip(terms) {
            *lane = lane.wrapping_add(u64::from_le_bytes(*bytes));
        }
        self.entries = self.entries.saturating_add(1);
    }
}

impl PartialEq for FolderDigest {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries && self.sum == other.sum
    }
}

impl Eq for FolderDigest {}

const fn kind_code(kind: PathEntryKind) -> u8 {
    match kind {
        PathEntryKind::Directory => 0,
        PathEntryKind::File => 1,
        PathEntryKind::Link => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Entry = (&'static str, PathEntryKind, Option<FileId>);

    fn digest(entries: &[Entry]) -> FolderDigest {
        let mut digest = FolderDigest::default();
        for (name, kind, identity) in entries {
            digest.add(OsStr::new(name), *kind, identity.as_ref());
        }
        digest
    }

    /// The entry `name`, which has the inode `number` on the first volume.
    fn entry(name: &'static str, kind: PathEntryKind, number: u64) -> Entry {
        (name, kind, Some(FileId::new_inode(1, number)))
    }

    #[test]
    fn the_same_entries_have_one_digest_in_any_order() {
        let listed = digest(&[
            entry("a", PathEntryKind::File, 1),
            entry("b", PathEntryKind::Directory, 2),
            entry("c", PathEntryKind::Link, 3),
        ]);
        let reversed = digest(&[
            entry("c", PathEntryKind::Link, 3),
            entry("b", PathEntryKind::Directory, 2),
            entry("a", PathEntryKind::File, 1),
        ]);

        assert_eq!(listed, reversed);
        assert_eq!(digest(&[]), FolderDigest::default());
    }

    /// Whatever the file system can do to a folder changes what it holds: an entry that is made,
    /// removed, renamed, or replaced by another of the same name, or one that is not what it
    /// was. Each must tell the digest apart from the one the map recorded.
    #[test]
    fn any_difference_in_the_entries_changes_the_digest() {
        let a = entry("a", PathEntryKind::File, 1);
        let b = entry("b", PathEntryKind::Directory, 2);
        let base = digest(&[a, b]);
        let changed: [(&str, Vec<Entry>); 8] = [
            ("made", vec![a, b, entry("c", PathEntryKind::File, 3)]),
            ("removed", vec![a]),
            ("renamed", vec![entry("z", PathEntryKind::File, 1), b]),
            (
                "replaced by another file of the name",
                vec![entry("a", PathEntryKind::File, 9), b],
            ),
            (
                "a file that is now a link",
                vec![entry("a", PathEntryKind::Link, 1), b],
            ),
            (
                "an identity the map did not record",
                vec![("a", PathEntryKind::File, None), b],
            ),
            (
                "an identity from another volume",
                vec![("a", PathEntryKind::File, Some(FileId::new_inode(2, 1))), b],
            ),
            ("the same entry listed twice", vec![a, a, b]),
        ];

        for (what, entries) in changed {
            assert_ne!(base, digest(&entries), "{what}");
        }
    }
}
