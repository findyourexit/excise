//! Builders for the tests of this module.

use std::collections::BTreeMap;

use crate::{
    report::{
        CountsCase, CountsContext, CountsFixture, CountsKind, CountsRunner, HarnessCounts,
        PullRequestOrigin, SchemaVersion,
    },
    scenario::Profile,
};

/// A fixture hash, and another one that differs from it.
pub(super) const HASH: &str = "3a7bd3e2360a3d29eea436fcfb7e44c735d117c42d1c1835420b6b9942dd4f1b";
pub(super) const OTHER_HASH: &str =
    "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

/// A commit that is `byte` repeated: 40 lowercase hexadecimal digits.
pub(super) fn commit(byte: u8) -> String {
    format!("{byte:02x}").repeat(20)
}

/// A case of `fixture` under the deterministic profile, with the counts `metrics`.
pub(super) fn case(fixture: &str, hash: &str, metrics: &[(&str, u64)]) -> CountsCase {
    CountsCase {
        fixture: CountsFixture {
            id: fixture.to_owned(),
            hash: hash.to_owned(),
            seed: 1,
        },
        profile: Profile::Deterministic,
        metrics: metrics
            .iter()
            .map(|(name, value)| ((*name).to_owned(), *value))
            .collect::<BTreeMap<_, _>>(),
    }
}

/// The record of `commit` taken on Linux, of `cases`.
pub(super) fn record(commit: &str, cases: Vec<CountsCase>) -> HarnessCounts {
    HarnessCounts {
        document_kind: CountsKind::HarnessCounts,
        schema_version: SchemaVersion,
        context: CountsContext {
            git_sha: commit.to_owned(),
            committed_at: "2026-10-05T10:35:18+11:00".to_owned(),
            runner: CountsRunner {
                os: "linux".to_owned(),
                os_version: "Ubuntu 24.04.3 LTS".to_owned(),
                arch: "x86_64".to_owned(),
            },
            toolchain: "rustc 1.98.0 (88d9e12ae 2026-08-18)".to_owned(),
            pull_request: None,
        },
        cases,
    }
}

/// `document` as the counts of pull request `number`, whose base is `base` and whose head is
/// `head`.
pub(super) fn for_pull_request(
    mut document: HarnessCounts,
    number: u64,
    base: &str,
    head: &str,
) -> HarnessCounts {
    document.context.pull_request = Some(PullRequestOrigin {
        number,
        base_sha: base.to_owned(),
        head_sha: head.to_owned(),
    });
    document
}

/// The cases of the usual suite, with the counts of a build that costs `store` bytes in the scan
/// store of every fixture.
pub(super) fn suite(store: u64) -> Vec<CountsCase> {
    vec![
        case(
            "wide-1k",
            HASH,
            &[
                ("entries", 1_002),
                ("residue_files", 0),
                ("scan_store_bytes", store),
            ],
        ),
        case(
            "tiny-files-50k",
            OTHER_HASH,
            &[
                ("entries", 49_051),
                ("residue_files", 0),
                ("scan_store_bytes", store * 49),
            ],
        ),
    ]
}
