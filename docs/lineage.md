# Project Lineage

!!! abstract "Independent successor"

    Excise descends from [Diskonaut](https://github.com/imsnif/diskonaut), created by Aram Drevekenin and improved by its contributors. It preserves technical ancestry while maintaining an independent name, release line, safety model, architecture, interface, maintenance policy, and distribution channels.

## At a Glance

Diskonaut
:   The upstream project whose history is preserved through commit `65cd829`.

Historical tags
:   Diskonaut releases `0.1.0` through `0.11.0`. They remain historical records and are never Excise releases.

Selected modernization work
:   The first Excise development line includes selected work from [`diskonaut-x`](https://github.com/kfkonrad/diskonaut), with original authorship retained.

Excise
:   An otherwise independent project. Preserving history does not imply endorsement by, or maintenance responsibility for, earlier contributors.

!!! warning "Do not reuse historical tags"

    Tags `0.1.0` through `0.11.0` describe Diskonaut releases. Do not move, reuse, or treat them as an Excise installation. Excise releases use their own tags, such as `v0.1.1` and the stable v1 line.

## Attribution and History

The repository preserves Diskonaut’s complete commit history through `65cd829`. Keeping that history visible preserves authorship and makes technical ancestry auditable; it does **not** merge the projects’ support or release obligations.

Use the repository history when exact attribution matters:

```console
# Inspect authorship and the preserved release history.
git log
git shortlog
git tag --list '0.*'
```

The MIT license and historical copyright notices remain in [LICENSE](../LICENSE).

??? info "Why the boundary matters"

    A preserved source history can explain where a design came from. It cannot establish a support promise, safety contract, release identity, or maintenance relationship for a later project. Those obligations are defined by Excise’s own documentation and release process.
