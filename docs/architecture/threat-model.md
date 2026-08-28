# Excise threat model

## Security and safety objective

Excise must not turn untrusted filesystem metadata, concurrent namespace changes, terminal capabilities, or user-interface ambiguity into unintended deletion, terminal injection, false completeness, or unbounded resource consumption.

## Trusted boundary

Excise trusts:

- the operating-system kernel and documented filesystem APIs;
- the local user account and explicit confirmation input;
- release artifacts after checksum/provenance verification;
- audited dependencies within the locked supply chain.

Excise does not claim to defend against a malicious kernel or a filesystem service that hangs or lies indefinitely.

## Hostile inputs

Treat as hostile:

- filenames containing controls, newlines, escapes, bidi markers, invalid UTF-8, or ill-formed UTF-16;
- symlinks, junctions, reparse points, mount points, and hard links;
- permission and sharing failures;
- files and directories replaced between scan, prompt, and mutation;
- children created after confirmation;
- extremely deep or flat directory trees;
- sparse/compressed files and shared identities;
- terminal resize/capability differences;
- invalid config, environment, and report paths;
- corrupted temporary spill state;
- dependency and release-pipeline compromise.

## Required controls

### Terminal injection

Store native paths losslessly. Render a reversible escaped display form. Never emit raw untrusted controls. Reduced width must not truncate away the escape/deception marker.

### Deletion confusion

- Complete/materialize subtree first.
- Build a snapshot of native identities.
- Revalidate identity/type/size/modification before consent and each mutation.
- Never follow links.
- Never sweep new identities into the plan.
- Refuse roots and synthetic nodes.
- Use typed or generated challenges.

### Resource exhaustion

- Bounded worker counts and channels.
- Hard model/index allocation budget.
- LRU exact aggregation and focused rescans.
- Iterative traversal/layout/destruction.
- Secure bounded identity spill.
- Effect manager with keyed replacement and no idle animation loop.

### Misleading output

- Explicit scan/aggregate/uncertain labels.
- Unknown values remain unknown.
- Hard-link and external-link uncertainty propagate into reclaimable bounds.
- Reflink/shared-extent limitation is disclosed.
- Headless/TUI schemas carry the same semantics.

### Terminal restoration

Validate before raw mode; use RAII restoration; test failures and panics in a PTY. Hard cancel restores terminal before terminating and explicitly accepts unknown partial mutation.

## Abuse cases

| Case | Required outcome |
|---|---|
| File becomes directory after prompt | Invalidate consent; rescan and re-prompt |
| New child appears during recursive delete | Do not delete it; report changed branch |
| Symlink points outside root | Display link; never traverse/delete target |
| Filename contains ESC sequence | Display escaped form; terminal state unaffected |
| Metadata query fails | Mark unknown; do not substitute apparent bytes |
| Flat directory exceeds budget | Keep largest entries plus exact `Other` |
| Repeated focus transitions | Replace keyed effect; memory remains bounded |
| Quit during deletion | Offer soft cancel, hard cancel, or back |

