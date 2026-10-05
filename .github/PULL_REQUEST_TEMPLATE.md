## Summary

<!-- What user-visible problem does this solve? -->

## Changes

<!-- Describe the observable behavior and important implementation choices. -->

## Safety and compatibility

<!-- Address deletion, path identity, accounting, terminal restoration, schemas, platforms, accessibility, and migration impact as applicable. -->

## Performance evidence

<!-- For a change that can affect performance (scanning, the scan store, rendering, deletion, memory, descriptors, threads): attach the `cargo xtask bench-e2e` JSON (the `harness-ab` document, `target/excise-bench-e2e/<run-id>/ab.json`) and its printed table, or say why none applies. CI comments the deterministic count deltas of a pull request that changes `src/`; those are counts, not timings. -->

## Verification

<!-- List exact commands and relevant manual scenarios with results. -->

- [ ] Focused tests cover new behavior and plausible regressions.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings` passes.
- [ ] `cargo test --workspace --locked` passes.
- [ ] User-facing documentation and generated artifacts are current.

Closes #
