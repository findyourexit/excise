# Security Policy

Excise permanently deletes files and folders. Data-loss defects are therefore security issues even when they do not cross a traditional privacy or permission boundary.

## Supported Versions

Excise supports the latest stable release, currently `1.2.0`. The `0.3.x` line was early testing and is superseded. Report safety defects against the affected stable release or development commit. Do not trust superseded releases with irreplaceable data.

| Version | Status |
|---|---|
| `1.2.0` | Supported stable line |
| `1.1.x` | Superseded stable releases. Upgrade to `1.2.0`. |
| `1.0.x` | Superseded stable releases. Upgrade to `1.2.0`. |
| `0.3.0` | Superseded early testing. Upgrade to `1.2.0`. |
| `0.2.0` | Superseded early testing. Upgrade to `1.2.0`. |
| `0.1.2` | Superseded early testing. Upgrade to `1.2.0`. |
| `0.1.1` | Superseded early testing. Upgrade to `1.2.0`. |
| `0.1.0` | Superseded early testing. Upgrade to `1.2.0`. |
| `main` | Development only |

## Report Privately

Use GitHub's **Security** tab and **Report a vulnerability** as the primary private reporting channel. Report issues such as:

- Unintended, over-broad, or incorrectly authorized deletion
- Path, link, junction, mount, or file identity confusion
- Race conditions that invalidate reviewed consent
- Terminal escape injection or failure to restore terminal state
- False claims about unique, shared, allocated, or reclaimable space
- Insecure temporary identity storage
- Dependency or release-integrity vulnerabilities
- Other confidentiality, integrity, or availability defects

If private vulnerability reporting is unavailable, email the lead maintainer at `tom.larcher@gmail.com`, the public maintainer contact listed in `Cargo.toml`. Do not open a public issue for an unpatched vulnerability or unintended-deletion path.

## What To Include

Provide the following when it is safe to do so:

- The affected commit or version and operating system
- Exact reproduction steps and commands
- Relevant file system, permission, mount, and link conditions
- Expected and actual behavior
- Whether deletion occurred
- A minimized synthetic fixture
- Any requested disclosure constraints

Do not include sensitive real file system paths when a synthetic reproduction is possible.

## Response Policy

Reports are handled on a best-effort basis. Maintainers assess severity, reproducibility, affected safety contracts, supported versions, and disclosure risk. Valid reports receive coordinated fixes and advisories when appropriate. No fixed response time is promised.

## Scope

Hostile names, links, permissions, and changes made during a scan are in scope. Excise trusts the operating-system kernel. A malicious or permanently unresponsive file system service is outside the current threat model. See [docs/architecture/threat-model.md](docs/architecture/threat-model.md).
