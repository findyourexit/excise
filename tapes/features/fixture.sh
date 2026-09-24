#!/usr/bin/env bash
# Creates a guarded, deterministic fixture for one README feature demo.
set -euo pipefail

scenario=${1:?usage: fixture.sh <scenario>}
case "$scenario" in
  storage-map|space-accounting|scoped-scanning|reviewed-deletion|background-work|reports|terminal-sessions|accessibility) ;;
  *)
    printf 'unknown feature demo scenario: %s\n' "$scenario" >&2
    exit 64
    ;;
esac

fixture="/tmp/excise-vhs-feature-${scenario}"
marker="$fixture/.excise-vhs-owned"
if [[ -e "$fixture" || -L "$fixture" ]]; then
  if [[ -f "$marker" ]]; then
    rm -rf -- "$fixture"
  else
    printf 'refusing unexpected fixture path: %s\n' "$fixture" >&2
    exit 1
  fi
fi

fixture_created=false

cleanup_fixture() {
  local status=$?
  if [[ $fixture_created == true ]]; then
    rm -rf -- "$fixture" || true
  fi
  exit "$status"
}
trap cleanup_fixture EXIT

umask 077
mkdir -m 700 "$fixture"
fixture_created=true

touch "$marker"
printf 'version = 1\n[runtime]\nformat = "tui"\n' > "$fixture/config.toml"

base_fixture() {
  mkdir -p "$fixture/media" "$fixture/backups" "$fixture/cache" "$fixture/downloads" "$fixture/projects/src"
  head -c 6291456 /dev/zero > "$fixture/media/capture.mov"
  head -c 3355443 /dev/zero > "$fixture/backups/snapshot.tar"
  head -c 2097152 /dev/zero > "$fixture/cache/index.bin"
  head -c 1258291 /dev/zero > "$fixture/downloads/toolchain.pkg"
  head -c 786432 /dev/zero > "$fixture/projects/target.bin"
  head -c 262144 /dev/zero > "$fixture/projects/src/main.rs"
  head -c 98304 /dev/zero > "$fixture/projects/src/lib.rs"
  head -c 340992 /dev/zero > "$fixture/notes.md"
}

case "$scenario" in
  storage-map|reports|terminal-sessions|accessibility)
    base_fixture
    ;;
  space-accounting)
    mkdir -p "$fixture/projects" "$fixture/archive"
    head -c 4194304 /dev/zero > "$fixture/projects/original.bin"
    ln "$fixture/projects/original.bin" "$fixture/projects/original-link.bin"
    head -c 2097152 /dev/zero > "$fixture/archive/snapshot.tar"
    truncate -s 8388608 "$fixture/projects/sparse.img"
    ;;
  scoped-scanning)
    mkdir -p "$fixture/keep" "$fixture/target"
    head -c 4194304 /dev/zero > "$fixture/keep/visible.bin"
    head -c 4194304 /dev/zero > "$fixture/target/ignored.bin"
    ;;
  reviewed-deletion)
    head -c 6291456 /dev/zero > "$fixture/delete-me.bin"
    head -c 131072 /dev/zero > "$fixture/keep-me.bin"
    ;;
  background-work)
    mkdir -p "$fixture/work"
    for number in $(seq 1 4096); do
      printf '%08d\n' "$number" > "$fixture/work/item-${number}.txt"
    done
    ;;
esac

printf '%s\n' "$fixture"
trap - EXIT
