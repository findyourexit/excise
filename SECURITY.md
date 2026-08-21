# Security policy

Excise performs permanent filesystem deletion, so data-loss defects are treated as security issues even when they do not cross a conventional confidentiality or privilege boundary.

## Supported versions

Excise supports the latest stable release, currently `0.1.0`. The `main` branch is development software and must not be trusted with irreplaceable data.

| Version | Status |
|---|---|
| `0.1.x` | Supported stable line |
| `main` | Development only |

## Report privately

Use GitHub's **Security** tab and **Report a vulnerability** to disclose:

- unintended, over-broad, or incorrectly authorized deletion;
- path, link, junction, mount, or file-identity confusion;
- race conditions that invalidate reviewed consent;
- terminal escape injection or failure to restore terminal state;
- materially false unique, shared, allocated, or reclaimable-space claims;
- insecure temporary identity storage;
- dependency or release-integrity vulnerabilities; and
- other confidentiality, integrity, or availability defects.

If private vulnerability reporting is unavailable, contact the lead maintainer through the methods listed on the [findyourexit GitHub profile](https://github.com/findyourexit). Do not open a public issue for an unpatched vulnerability or unintended-deletion path.

## What to include

Provide, where safe:

- the affected commit or version and operating system;
- exact reproduction steps and commands;
- relevant filesystem, permission, mount, and link conditions;
- expected and actual behavior;
- whether deletion occurred;
- a minimized synthetic fixture; and
- any requested disclosure constraints.

Do not include sensitive real filesystem paths when a synthetic reproduction is possible.

## Response policy

Reports are handled on a best-effort basis. Maintainers assess severity, reproducibility, affected safety contracts, supported versions, and disclosure risk. Valid reports receive coordinated fixes and advisories when appropriate. No fixed response SLA is promised.

## Scope

Hostile filenames, links, permissions, and concurrent namespace mutation are in scope. Excise trusts the operating-system kernel; a malicious or permanently unresponsive filesystem implementation is outside the current threat model. See [docs/architecture/threat-model.md](docs/architecture/threat-model.md).
