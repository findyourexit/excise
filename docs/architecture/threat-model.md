# Excise Threat Model

## Safety Objective

Excise must not turn untrusted file information, changes made while a scan is running, terminal behavior, or an unclear interface into unintended deletion, terminal control, a false claim of completeness, or unbounded resource use.

## What Excise Trusts

Excise trusts the operating system kernel and its documented file system interfaces. It trusts the local user account and explicit confirmation input. It trusts release files after their checksums and origin have been verified. It trusts the locked set of reviewed dependencies.

Excise does not claim to defend against a malicious kernel or a file system service that hangs or reports false information forever.

## Hostile Inputs

Excise treats the following as hostile:

- Names containing control characters, newlines, escape sequences, bidirectional text marks, invalid UTF-8, or invalid UTF-16
- Symbolic links, junctions, reparse points, mount points, and files with more than one name
- Permission and file-sharing failures
- Files and folders replaced between scanning, confirmation, and deletion
- Children created after confirmation
- Extremely deep or very wide directory trees
- Sparse files, compressed files, and files that share storage
- Terminal size and capability changes
- Invalid configuration, environment, and report paths
- Corrupted temporary session data
- Compromised dependencies or release systems

## Required Controls

### Terminal Injection

Store file paths without losing their original bytes. Show a reversible escaped form. Never write untrusted control characters directly to the terminal. A narrow display must keep the marker that warns about deceptive text.

### Deletion Confusion

- Fully examine the selected folder before offering deletion.
- Record the file identities in the review plan.
- Check identity, type, size, modification time, and allocation before asking for confirmation and before each deletion.
- Never follow a link to its target.
- Never add a new identity to the plan.
- Refuse file system roots and summary entries.
- Use a typed confirmation or a generated challenge when the name could be misleading.

### Resource Exhaustion

- Limit worker counts and queues.
- Enforce a hard memory limit for the working model and index.
- Keep exact totals with a bounded store and focused rescans.
- Use loops for traversal, layout, and deletion.
- Store identity data securely when it does not fit in memory.
- Replace repeated visual effects by purpose and avoid an idle animation loop.

### Misleading Output

- Label scan, summary, and uncertainty states clearly.
- Keep unknown values unknown.
- Carry uncertainty from hard-link observations and links outside the scan scope into reclaimable totals.
- Explain the limits of physical shared-storage accounting.
- Give interactive and noninteractive reports the same meaning.

### Terminal Restoration

Validate the terminal before entering raw input mode. Restore it automatically on normal exit, errors, panics, and cancellation. Test failures and panics through a pseudo-terminal. A forced cancellation restores the terminal before exit and reports that the final deletion state may be unknown.

## Abuse Cases

| Situation | Required outcome |
|---|---|
| A file becomes a directory after confirmation | Cancel the plan and request a new scan |
| A new child appears during recursive deletion | Leave it untouched and report the changed folder |
| A symbolic link points outside the selected root | Display the link and never traverse or delete its target |
| A name contains an escape sequence | Display an escaped name and leave terminal state unchanged |
| A metadata query fails | Mark the value unknown and do not substitute file length |
| A flat directory exceeds the memory limit | Keep the largest entries and an exact `Other` summary |
| Focus changes repeat quickly | Replace the earlier visual effect and keep memory bounded |
| The user quits during deletion | Offer a soft cancel, a forced cancel, or a return to deletion |

