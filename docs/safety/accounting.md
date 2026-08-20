# Storage accounting contract

## Headline metric

Excise reports **identity-unique allocated bytes** as its primary size.

- Count regular-file allocation once per filesystem identity.
- Count symlink/reparse-object allocation where the platform exposes it.
- Exclude directory metadata allocation for consistent cross-platform semantics.
- Keep apparent/logical bytes as a separate metric.
- Never silently replace unknown allocated bytes with apparent bytes.

## Hard links

Track observed identity, allocated bytes, declared link count where available, and observed paths.

- Deduplicate identity allocation globally within scan scope.
- Linked path entries remain visible but own zero additional unique bytes.
- Allocation referenced from multiple child subtrees appears under a synthetic `Shared` node at the observed lowest common ancestor.
- `Shared` is informational and never deletable.

## Reclaimable bounds

For each real subtree, calculate:

- known identity-unique allocation;
- conservative lower bound of bytes reclaimable by deleting the planned subtree;
- upper bound where link observations permit one;
- unknown contribution and unknown-entry/subtree counts.

Links that may exist outside scan scope prevent false exactness.

## Unknown data

A failed metadata/allocation query contributes unknown bytes. An unreadable directory contributes unbounded unknown descendants. Ancestors remain lower-bounded and visibly uncertain.

Expected user exclusions and one-filesystem boundaries define scope; report those boundaries separately instead of treating them as read failures.

Configured user exclusions and foreign-filesystem boundaries remain visible as zero-byte scoped records with their reason. Verified Excise-owned session and scanner-queue paths are a different class: they are omitted silently before model insertion, never enter user accounting or reports, and are matched only by their exact active native paths. Similarly named user content is scanned normally.

## Shared extents limitation

Version 1.0 deduplicates filesystem identities, not physical extents. Reflinks, clones, transparent filesystem deduplication, and compressed/shared extents can still cause overcounting. This limitation appears in documentation and relevant report metadata.

## Memory and spill

Hard-link identity tracking may spill minimum identity/accounting data into a permission-restricted, session-only temporary store. If exact tracking cannot continue securely, stop with an actionable failure rather than weaken the accounting definition.

## Treemap invariants

- Child geometry adds to the parent's represented known allocation.
- `Shared` and `Other` preserve additive totals.
- Unknown contribution is represented outside falsely numeric area.
- Geometry is finite, deterministic, in bounds, and non-overlapping.
- Animation interpolates between valid old/new geometries and never changes the underlying numbers.

## Required fixtures

- multiple links in one directory;
- links split across sibling/deep subtrees;
- links outside selected subtree and outside scan scope;
- sparse and compressed files;
- unreadable allocation metadata;
- inaccessible directories;
- zero and maximum-size values;
- aggregation and identity-spill pressure;
- platform-specific identity providers.
