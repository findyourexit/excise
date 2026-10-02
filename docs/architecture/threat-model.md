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
- Entries in the scratch parent that are named like a session directory but are links, belong to another user, or were never made by Excise.
- Compromised dependencies or release systems.

## Required Controls

=== "Terminal and display"

    **Terminal injection** — Store file paths without losing original bytes. Show a reversible escaped form. Never write untrusted control characters directly to the terminal. A narrow display keeps the marker that warns about deceptive text.

    **Terminal restoration** — Validate the terminal before entering raw input mode. Restore it automatically on normal exit, typed errors, panics, cancellation, and an external SIGTERM, SIGHUP, or SIGQUIT on Unix, or a console close, break, logoff, or shutdown event on Windows, ordered after every frame byte already produced. The wait for a terminal that is merely slow to absorb that output is bounded, not unbounded, so a terminal that never reads anything cannot hang exit; raw mode is still disabled either way. Test failures and panics through a pseudo-terminal. An active deletion stops only at an entry boundary, and its worker always joins before the terminal session ends. Any of those signals or console events is a confirmed quit: it cancels pending plans, stops an active deletion at its next entry boundary, removes session storage, and exits with the `Interrupted` class (130); a second one forces that exit instead of waiting out anything the first started. Shutdown waits for each thread it stops. A second signal bounds the wait for the scanner, the deletion planner, an export in progress, and the scan-store thread: a thread inside a call that a hung file system never answers cannot be stopped from outside, so each gets one second to end, and then the terminal is restored and the process exits without it. It does not bound the wait for the deletion worker, which still joins first: an entry is moved aside before it is removed, so ending the process inside one could leave a target that is neither intact nor gone, and a deletion stuck in a call that never returns holds a forced exit. A thread left behind keeps its session directory, which the next start's sweep removes.

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
    - Store scan data in private files with fixed limits, in a session directory that only the current user can open.
    - Remove a dead session's directory only when it is verified, unlocked, and the current user's. Each session holds a lock on a file in its own directory for its whole life, and that file records that Excise made the directory. A start removes a `.excise-scan-*` directory only when it is a real directory on the scratch parent's file system, never a link, owned by the current user and closed to everyone else; its lock file is a regular file under the same rules and holds exactly that record; and the lock can be taken, so no process holds it. It removes inside that directory without following a link or leaving it, leaves a directory it cannot remove completely verifiable for the next start, and leaves everything else alone. It never reports and never fails the run that started it.
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
| A previous session was killed and left its scratch directory behind | The next start removes the directory when it is verified, unlocked, and the current user's. A running session, including one that starts at the same moment, is never removed. |
| A scratch directory is named like a session but is a link, belongs to another user, is open to others, or has no valid record that Excise made it | Leave it exactly as found. Never follow a link and never remove anything inside it. |
| Focus changes repeat quickly | Replace the earlier visual effect and keep memory bounded. |
| The user quits during deletion | Cancel pending plans, safely stop after the current entry, or return to the map and wait. Never detach an active filesystem change. |

??? info "Security review lens"

    A change is not safe merely because it passes a happy-path test. Review how it handles hostile names, replacement races, partial retained state, capacity failures, interruption, and display ambiguity. The required outcome is explicit failure or uncertainty, never a confident guess.
