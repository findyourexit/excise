#!/usr/bin/env bash
# Refuses a pull request's counts artifact before it is downloaded, unless the run that made it
# uploaded exactly one artifact, named `pr-counts`, that is no larger than a counts artifact can
# be.
#
# The artifact is made by the pull request's own workflow run, so its size is the author's choice,
# and `actions/download-artifact` unpacks all of it onto the runner before anything can read it.
# The size that GitHub reports is the compressed one, and that is what bounds the unpacked one
# too (deflate expands by about a thousand at most), so a bound on it bounds the disk and the
# time that the download can spend.
#
#   REPOSITORY  owner/name of this repository (from the workflow)
#   RUN_ID      the id of the run that uploaded the artifact (from the workflow payload)
#   GH_TOKEN    the workflow's token, read by `gh`
#
# Nothing here is interpolated into a command: the two values are validated and then quoted, and
# what the listing holds is only compared, never printed or run.

set -euo pipefail

# The one artifact that `Pull-request counts` uploads. The name is part of the workflows' contract.
readonly NAME='pr-counts'
# The most a counts document may be (`MAX_DOCUMENT_BYTES` of `crates/excise-harness/src/counts/
# artifact.rs`; a test keeps the two equal). The document is JSON text, which compresses, so this
# is a generous bound for a real artifact and a small one for what a download could be made to
# unpack.
readonly MAX_BYTES=65536

fail() {
  echo "check-count-artifact: $1" >&2
  exit 1
}

[[ "${REPOSITORY:-}" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || fail 'REPOSITORY is not owner/name'
[[ "${RUN_ID:-}" =~ ^[1-9][0-9]{0,18}$ ]] || fail 'RUN_ID is not a run id'

# `total_count` is the number of artifacts the run has, wherever they are listed, so a run that
# uploads many cannot hide the rest on a later page.
artifacts="$(gh api "repos/${REPOSITORY}/actions/runs/${RUN_ID}/artifacts")"
reason="$(
  jq -r --arg name "$NAME" --argjson max "$MAX_BYTES" '
    (.artifacts // []) as $all
    | if .total_count != 1 or ($all | length) != 1 then
        "the run uploaded \(.total_count // "an unknown number of") artifacts: exactly one, named \($name), is expected"
      elif $all[0].name != $name then
        "the one artifact of the run is not named \($name)"
      elif ($all[0].expired // false) then
        "the artifact has expired"
      elif ($all[0].size_in_bytes | type) != "number" or $all[0].size_in_bytes > $max then
        "the artifact is larger than \($max) bytes compressed, or has no size"
      else
        empty
      end' <<<"$artifacts"
)"
[[ -z "$reason" ]] || fail "$reason"
echo "The run uploaded one artifact, ${NAME}, within ${MAX_BYTES} bytes: it may be downloaded."
