# Governance

## Maintainer Model

Excise currently uses a lead maintainer model. The lead maintainer has final responsibility for product direction, safety, releases, and project administration. Delegated authority is recorded in [MAINTAINERS.md](MAINTAINERS.md).

Until a second maintainer is appointed, the lead maintainer may approve stable releases and material safety or release-trust changes after public review, evidence, protected-main ruleset checks, and publication-environment approval. This is the current release authority. It does not imply that a second maintainer exists.

When a second maintainer is appointed:

- Stable releases require approval from another maintainer.
- Changes to deletion, path identity, space accounting, terminal restoration, security, or release trust require another maintainer's approval.
- Maintainers may not approve their own elevation or expansion of authority.

## Decisions

Public issues and pull requests are the project record. Material changes should state the user problem, the alternatives considered, compatibility impact, safety impact, and verification evidence. Maintainers may request a design issue before implementation when a change affects a public contract or is difficult to reverse.

Routine implementation detail belongs in focused commits and pull requests rather than permanent policy documents.

## Releases

Maintainers publish releases only from protected history after supported-platform, safety, test, dependency, packaging, and origin-record checks pass for the exact release commit. Release credentials and publication authority remain separate from ordinary contributor access.

## Maintainer Changes

Maintainers are selected for sustained technical judgment, constructive review, reliability, and demonstrated care for Excise's safety contracts. The lead maintainer proposes changes publicly. Once multiple maintainers exist, adding or removing a maintainer requires approval from at least one unaffected maintainer.

## Succession

The repository should remain ready for transfer. Moving it to a dedicated organization requires a public proposal, a continuity plan for releases and security reporting, and approval from the lead maintainer plus one other maintainer when available.

## Amendments

Governance changes use the same public review process as other material project changes. The merging pull request must update this file.
