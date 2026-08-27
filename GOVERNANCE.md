# Governance

## Maintainer model

Excise currently uses a lead-maintainer model. The lead maintainer has final responsibility for product direction, safety, releases, and project administration. Delegated authority is recorded in [MAINTAINERS.md](MAINTAINERS.md).
Until a second maintainer is appointed, the lead maintainer may approve stable releases and material safety or release-trust changes after the required public review, evidence, protected-main ruleset checks, and publication-environment approval. This is the current release authority; it does not imply that a second maintainer exists.

When a second maintainer is appointed:

- stable releases require approval from another maintainer;
- changes to deletion, path identity, storage accounting, terminal restoration, security, or release trust require another maintainer's approval; and
- maintainers may not approve their own elevation or expansion of authority.

## Decisions

Public issues and pull requests are the project record. Material changes should state the user problem, alternatives considered, compatibility impact, safety impact, and verification evidence. Maintainers may request a design issue before implementation when a change affects public contracts or is difficult to reverse.

Routine implementation detail belongs in focused commits and pull requests rather than permanent policy documents.

## Releases

Maintainers publish releases only from protected history after the supported-platform, safety, test, dependency, packaging, and provenance checks pass for the exact release commit. Release credentials and publication authority remain separate from ordinary contributor access.

## Maintainer changes

Maintainers are selected based on sustained technical judgment, constructive review, reliability, and demonstrated care for Excise's safety contracts. The lead maintainer proposes changes publicly. Once multiple maintainers exist, adding or removing a maintainer requires approval from at least one unaffected maintainer.

## Succession

The repository should remain transfer-ready. Moving it to a dedicated organization requires a public proposal, a continuity plan for releases and security reporting, and approval from the lead maintainer plus one other maintainer when available.

## Amendments

Governance changes use the same public review process as other material project changes and must update this file in the merging pull request.
