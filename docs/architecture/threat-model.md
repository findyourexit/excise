# Excise Threat Model

!!! danger "Safety objective"

    Excise must not turn untrusted file information, changes made while a scan is running, terminal behavior, or an unclear interface into unintended deletion, terminal control, a false claim of completeness, or unbounded resource use.

## Trust Boundary

| Excise trusts | Excise does not claim to defend against |
|---|---|
| The operating-system kernel and its documented filesystem interfaces | A malicious kernel |
| The local user account and explicit confirmation input | A filesystem service that hangs or reports false information forever |
| Release files after checksum and origin verification | — |
| The locked set of reviewed dependencies | — |

## Treat These Inputs as Hostile

- Names containing control characters, newlines, escape sequences, bidirectional text marks, invalid UTF-8, or invalid UTF-16.
- Symbolic links, junctions, reparse points, mount points, and files with more than one name.
- Permission and file-sharing failures.
- Files and folders replaced between scanning, confirmation, and deletion.
- Children created after confirmation.
- Extremely deep or very wide directory trees.
- Sparse files, compressed files, and files that share storage.
- Terminal size and capability changes.
- Invalid configuration, environment, and report paths.
- Corrupted temporary session data.
- Compromised dependencies or release systems.

## Required Controls

=== "Terminal and display"

    **Terminal injection** — Store file paths without losing original bytes. Show a reversible escaped form. Never write untrusted control characters directly to the terminal. A narrow display keeps the marker that warns about deceptive text.

    **Terminal restoration** — Validate the terminal before entering raw input mode. Restore it automatically on normal exit, typed errors, panics, and cancellation. Test failures and panics through a pseudo-terminal. An active deletion stops only at an entry boundary, and its worker always joins before the terminal session ends.

=== "Deletion"

    - Fully examine the selected folder before offering deletion.
    - Record identities in the review plan.
    - Check identity, type, size, modification time, and allocation before confirmation and before each deletion.
    - Never follow a link to its target.
    - Never add a new identity to the plan.
    - Refuse filesystem roots and summary entries.
    - Use typed confirmation or a generated challenge when a name could be misleading.

=== "Resources and storage"

    - Limit worker counts and queues.
    - Enforce a hard memory limit for page views and a separate scan-store quota.
    - Keep exact totals in private stored scan data and immutable page queries.
    - Use loops for traversal, layout, and deletion.
    - Store scan data in private files with fixed limits.
    - Replace repeated visual effects by purpose and avoid an idle animation loop.

=== "Reports and meaning"

    - Label scan, summary, and uncertainty states clearly.
    - ==Keep unknown values unknown.==
    - Carry uncertainty from hard-link observations and links outside the scan scope into reclaimable totals.
    - Explain physical shared-storage accounting limits.
    - Give interactive and noninteractive reports the same meaning.

## Abuse Cases

| Situation | Required outcome |
|---|---|
| A file becomes a directory after confirmation | Reject the stale plan and require a fresh user request. |
| A new child appears during recursive deletion | Leave it untouched and report the changed folder. |
| A symbolic link points outside the selected root | Display the link and never traverse or delete its target. |
| A name contains an escape sequence | Display an escaped name and leave terminal state unchanged. |
| A metadata query fails | Mark the value unknown and do not substitute file length. |
| The scan store reaches capacity | Publish a deterministic `summary-only` result. Do not expose a detailed map or deletion controls. |
| Focus changes repeat quickly | Replace the earlier visual effect and keep memory bounded. |
| The user quits during deletion | Cancel pending plans, safely stop after the current entry, or return to the map and wait. Never detach an active filesystem change. |

??? info "Security review lens"

    A change is not safe merely because it passes a happy-path test. Review how it handles hostile names, replacement races, partial retained state, capacity failures, interruption, and display ambiguity. The required outcome is explicit failure or uncertainty, never a confident guess.
