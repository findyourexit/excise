# bench-data

The count history of `excise`, written only by the `Count history` workflow on every push to
`main`, and read by the `Pull-request count comment` workflow.

Each commit of `main` has at most one record, `records/<os>/<first two hex digits of the
commit>/<commit>.json`: a `harness-counts` document, described by
`crates/excise-harness/schemas/harness-counts.schema.json` on `main`. A record is never edited.
Do not edit this branch by hand. See `docs/development.md` on `main`, "Counts and count history".
