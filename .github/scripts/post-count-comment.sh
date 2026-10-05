#!/usr/bin/env bash
# Posts the one comment that carries a pull request's deterministic counts, or updates it.
#
# This is the only step of the pull-request count workflows that writes to a pull request, and it
# runs in the trusted workflow (`workflow_run`, the default branch's copy). Of what it is given,
# only the pull request number, the base commit, and the Markdown come from the pull request's
# run (through an artifact that `xtask counts-comment` has validated), and this checks them again:
#
#   REPOSITORY         owner/name of this repository (from the workflow, not the artifact)
#   PR_NUMBER          the pull request number, which `xtask counts-comment` took from the
#                      artifact and validated, and which this checks against the API below
#   BASE_SHA           the base commit the counts were compared with, which `xtask counts-comment`
#                      took from the artifact and validated, and which this checks against the
#                      API below
#   EXPECTED_HEAD_SHA  the head commit of the run that uploaded the artifact (from the workflow
#                      payload, not the artifact)
#   HEAD_REPOSITORY    owner/name of the repository that head commit was pushed to (from the
#                      workflow payload)
#   HEAD_BRANCH        the branch it was pushed to (from the workflow payload)
#   RUN_PULL_REQUESTS  the numbers of the pull requests that the payload lists for the run, comma
#                      separated. It must be set, and it is empty where GitHub lists none, which
#                      is the case for a pull request from a fork: the head and base bindings
#                      below then tell the pull requests apart (GitHub allows no two open pull
#                      requests with the same head and base)
#   BODY_FILE          the Markdown that `xtask counts-comment` rendered
#   GH_TOKEN           the workflow's token, read by `gh`
#
# Nothing here is interpolated into a command: every value is validated, and then only quoted
# into arguments. The comment's body travels as a file.

set -euo pipefail

# The first line of every comment this posts, by which it is found again. It is the MARKER of
# `crates/excise-harness/src/counts/comment.rs`; a test keeps the two equal.
readonly MARKER='<!-- excise-counts -->'
# The account that a workflow's GITHUB_TOKEN comments as. A comment by anyone else that begins
# with the marker is not ours and is left alone.
readonly BOT='github-actions[bot]'

fail() {
  echo "post-count-comment: $1" >&2
  exit 1
}

[[ "${REPOSITORY:-}" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || fail 'REPOSITORY is not owner/name'
[[ "${PR_NUMBER:-}" =~ ^[1-9][0-9]{0,15}$ ]] || fail 'PR_NUMBER is not a pull request number'
[[ "${BASE_SHA:-}" =~ ^[0-9a-f]{40}$ ]] || fail 'BASE_SHA is not a full commit'
[[ "${EXPECTED_HEAD_SHA:-}" =~ ^[0-9a-f]{40}$ ]] || fail 'EXPECTED_HEAD_SHA is not a full commit'
[[ "${HEAD_REPOSITORY:-}" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || fail 'HEAD_REPOSITORY is not owner/name'
[[ -n "${HEAD_BRANCH:-}" && "$HEAD_BRANCH" != *[[:cntrl:]]* ]] || fail 'HEAD_BRANCH is empty or has control characters'
[[ -n "${RUN_PULL_REQUESTS+set}" ]] || fail 'RUN_PULL_REQUESTS is not set (it is empty where the payload lists no pull request)'
[[ -z "$RUN_PULL_REQUESTS" || "$RUN_PULL_REQUESTS" =~ ^[1-9][0-9]{0,15}(,[1-9][0-9]{0,15})*$ ]] || fail 'RUN_PULL_REQUESTS is not a list of pull request numbers'
[[ -n "${BODY_FILE:-}" && -f "$BODY_FILE" && ! -L "$BODY_FILE" ]] || fail 'BODY_FILE is not a regular file'

# Where the payload lists the run's pull requests, the artifact may only name one of them. (An
# artifact is whatever the pull request's code made, so it can name any number.)
if [[ -n "$RUN_PULL_REQUESTS" && ",${RUN_PULL_REQUESTS}," != *",${PR_NUMBER},"* ]]; then
  fail "the counts name pull request #${PR_NUMBER}, which is not one of the pull requests this run was for (${RUN_PULL_REQUESTS})"
fi

# The pull request must exist in this repository, be open, still be at the commit whose counts
# these are, from the repository and the branch that the counted run came from, and still be
# based on the commit they were compared against. The number came from an artifact that the pull
# request's author controls, so the run is what binds it: an artifact cannot comment on another
# pull request unless that pull request has the same head commit in the same repository and
# branch, and the same base, that is, is the same change. (The head commit alone does not say
# which pull request was counted: one commit can be the head of a contributor's pull request and
# of a maintainer's. The base commit is what the comparison was made against, so it is also what
# a stale run, or an artifact that names a base of its own choosing, would get wrong.)
pull="$(gh api "repos/${REPOSITORY}/pulls/${PR_NUMBER}")"
state="$(jq -r '.state' <<<"$pull")"
head="$(jq -r '.head.sha' <<<"$pull")"
head_repository="$(jq -r '.head.repo.full_name // ""' <<<"$pull")"
head_branch="$(jq -r '.head.ref // ""' <<<"$pull")"
base="$(jq -r '.base.sha // ""' <<<"$pull")"
if [[ "$state" != 'open' ]]; then
  echo "Pull request #${PR_NUMBER} is not open: no comment."
  exit 0
fi
if [[ "$head" != "$EXPECTED_HEAD_SHA" ]]; then
  echo "Pull request #${PR_NUMBER} has moved past the commit that was counted: the run for its new head comments."
  exit 0
fi
if [[ "$head_repository" != "$HEAD_REPOSITORY" || "$head_branch" != "$HEAD_BRANCH" ]]; then
  echo "Pull request #${PR_NUMBER} is not from the repository and branch that were counted: no comment."
  exit 0
fi
if [[ "$base" != "$BASE_SHA" ]]; then
  echo "Pull request #${PR_NUMBER} has a different base commit than the one that was counted: no comment."
  exit 0
fi

# The comment this posted before: the first by the bot that begins with the marker, on any page.
id="$(
  gh api --paginate "repos/${REPOSITORY}/issues/${PR_NUMBER}/comments?per_page=100" |
    jq -rs --arg marker "$MARKER" --arg bot "$BOT" \
      '[.[][] | select(.user.login == $bot and (.body | startswith($marker)))] | .[0].id // empty'
)"

if [[ -n "$id" ]]; then
  [[ "$id" =~ ^[0-9]+$ ]] || fail 'the existing comment has no numeric id'
  gh api --method PATCH "repos/${REPOSITORY}/issues/comments/${id}" --field "body=@${BODY_FILE}" >/dev/null
  echo "Updated comment ${id} on pull request #${PR_NUMBER}."
else
  gh api --method POST "repos/${REPOSITORY}/issues/${PR_NUMBER}/comments" --field "body=@${BODY_FILE}" >/dev/null
  echo "Commented on pull request #${PR_NUMBER}."
fi
