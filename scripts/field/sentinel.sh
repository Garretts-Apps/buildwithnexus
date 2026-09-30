#!/usr/bin/env bash
# Scheduled sentinel for repo-level failures that no PR workflow sees: a
# version-bump merge whose push event never started release.yml (U22), a red
# main or a main commit CI never ran on (U03), the docs site behind npm (U05),
# advisories in Cargo.lock, Dependabot labels and PRs nobody looked at (U01),
# and runner labels GitHub is retiring. Run by .github/workflows/sentinel.yml.
#
# Usage: scripts/field/sentinel.sh [--dry-run] [--checks frequent|daily|all|NAME,...]
#                                  [--repo OWNER/NAME] [--root DIR]
#
# Prints PASS/FAIL/SKIP/ERROR per check. Each failure signature gets one issue
# labelled "sentinel"; the issue closes itself after its check passes 3 runs
# in a row. Closing an issue as "not planned" mutes that signature.
#
# Exit status is 0 unless the sentinel itself is broken, so a red run means
# "fix the sentinel", never "look at the repo":
# - FAIL opens or updates an issue and does not fail the run.
# - A check that hits a network or API hiccup (no response, timeout, HTTP
#   408/429/5xx, rate limit) is retried once, then reported as a transient
#   ERROR row. Its issues are left as they are, and the run still exits 0.
# - Exit 1: a check crashed with no network cause, or GitHub rejected a
#   request the sentinel built (any other 4xx: a wrong path or a permission
#   sentinel.yml does not grant).
#
# GitHub access is the REST API through curl, with GH_TOKEN (or GITHUB_TOKEN)
# when set. Without a token only the read-only checks work, so it runs as
# --dry-run. Needs bash 4.4+, curl and jq; the audit check needs cargo-audit.
set -euo pipefail
shopt -s inherit_errexit

FREQUENT_CHECKS="release-gap main-red site-version"
DAILY_CHECKS="audit labels stale-bot-prs runner-labels"
CLOSE_AFTER=3          # passing runs in a row before an issue closes
RELEASE_GRACE_MIN=10   # a push starts release.yml within seconds when it works
PUBLISH_START_MIN=20   # publish.yml chains within a minute of a successful release
PUBLISH_GRACE_MIN=30   # after publish.yml succeeds, for npm to show the version
CI_GRACE_MIN=30
SITE_GRACE_MIN=120
BOT_PR_MAX_DAYS=3
RETRY_DELAY=${SENTINEL_RETRY_DELAY:-20}  # seconds before a check's one retry
LABEL=sentinel

API=${GITHUB_API_URL:-https://api.github.com}
SERVER=${GITHUB_SERVER_URL:-https://github.com}
NPM_REGISTRY=${SENTINEL_NPM_REGISTRY:-https://registry.npmjs.org}
SITE_URL=${SENTINEL_SITE_URL:-https://buildwithnexus.dev}
BRANCH=${SENTINEL_BRANCH:-main}
# Backtests: SENTINEL_NOW (epoch) and SENTINEL_HEAD (a commit) evaluate the
# frequent checks as of an earlier moment; runs created later are ignored.
NOW=${SENTINEL_NOW:-$(date -u +%s)}
TOKEN=${GH_TOKEN:-${GITHUB_TOKEN:-}}

usage() { sed -n '/^# Usage:/,/^#$/{/^#$/d;s/^# \{0,1\}//;p}' "$0"; }
die() { echo "sentinel: $*" >&2; exit 2; }
log() { printf '%s\n' "$*"; }

DRY_RUN=0 CHECKS=all REPO=${GITHUB_REPOSITORY:-} ROOT=
while (($#)); do
  case $1 in
    --dry-run) DRY_RUN=1 ;;
    --checks) CHECKS=${2:?--checks needs a value}; shift ;;
    --checks=*) CHECKS=${1#*=} ;;
    --repo) REPO=${2:?--repo needs a value}; shift ;;
    --repo=*) REPO=${1#*=} ;;
    --root) ROOT=${2:?--root needs a value}; shift ;;
    --root=*) ROOT=${1#*=} ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; die "unknown argument: $1" ;;
  esac
  shift
done
ROOT=${ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}
[[ $REPO =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || die "pass --repo OWNER/NAME (or set GITHUB_REPOSITORY)"
command -v jq >/dev/null || die "jq is required"
command -v curl >/dev/null || die "curl is required"

case $CHECKS in
  frequent) SELECTED=$FREQUENT_CHECKS ;;
  daily) SELECTED=$DAILY_CHECKS ;;
  all) SELECTED="$FREQUENT_CHECKS $DAILY_CHECKS" ;;
  *)
    SELECTED=${CHECKS//,/ }
    for c in $SELECTED; do
      [[ " $FREQUENT_CHECKS $DAILY_CHECKS " == *" $c "* ]] || die "unknown check: $c"
    done
    ;;
esac

NPM_PKG=${SENTINEL_NPM_PACKAGE:-$(jq -r '.name // empty' "$ROOT/package.json" 2>/dev/null || true)}
NPM_PKG=${NPM_PKG:-buildwithnexus}

AUTH=()
if [[ -n $TOKEN ]]; then
  AUTH=(-H "Authorization: Bearer $TOKEN")
  VIA="curl with token"
else
  VIA="curl, no token (read-only)"
fi
if ((!DRY_RUN)) && [[ -z $TOKEN ]]; then
  echo "::warning::sentinel: no GH_TOKEN, so nothing can be written; running as --dry-run" >&2
  DRY_RUN=1
fi
if [[ -n ${GITHUB_RUN_ID:-} ]]; then
  RUN_LINK="[run $GITHUB_RUN_ID]($SERVER/$REPO/actions/runs/$GITHUB_RUN_ID)"
else
  RUN_LINK="a local run"
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
: >"$WORK/results"
: >"$WORK/actions"
: >"$WORK/dispatch"
: >"$WORK/neterr"
BUGS=0  # sentinel bugs seen; any makes the run exit 1

# ---------------------------------------------------------------- transport

# net_error KIND TEXT: note a failed request. KIND is "transient" (worth a
# retry, never fails the run) or "permanent" (the sentinel asked for something
# wrong). The runner classifies a crashed check by the last note.
net_error() {
  printf '%s\t%s\n' "$1" "$2" >>"$WORK/neterr"
  echo "$2" >&2
}

# error_kind: "transient" or "sentinel bug", from the last net_error.
error_kind() {
  if [[ $(tail -n1 "$WORK/neterr" | cut -f1) == transient ]]; then echo transient; else echo "sentinel bug"; fi
}

# fetch WHAT CURL-ARGS...: body on stdout when the response is 2xx.
fetch() {
  local what=$1 body code rc=0 kind=permanent
  shift
  body=$(mktemp "$WORK/body.XXXXXX")
  code=$(curl -sS --max-time 30 -o "$body" -w '%{http_code}' "$@") || rc=$?
  if ((rc == 0)) && [[ $code == 2?? ]]; then
    cat "$body"
    return 0
  fi
  # curl: 5/6 cannot resolve, 7 cannot connect, 16/92 HTTP/2, 18 partial,
  # 28 timeout, 35 TLS, 52 empty reply, 55/56 send/receive.
  case $rc in 5 | 6 | 7 | 16 | 18 | 28 | 35 | 52 | 55 | 56 | 92) kind=transient ;; esac
  case $code in 408 | 429 | 5??) kind=transient ;; esac
  if [[ $code == 403 ]] && grep -qi 'rate limit' "$body"; then kind=transient; fi
  if ((rc)); then
    net_error "$kind" "$what: curl exit $rc"
  else
    net_error "$kind" "$what: HTTP $code $(jq -r '.message // empty' "$body" 2>/dev/null | head -c 200 || true)"
  fi
  return 1
}

gh_get() { # PATH ACCEPT
  fetch "GET $1" --retry 1 --retry-delay 2 -H "Accept: $2" -H 'X-GitHub-Api-Version: 2022-11-28' \
    -H 'User-Agent: bwn-sentinel' "${AUTH[@]}" "$API/$1"
}

# api PATH: GET a REST path, JSON on stdout.
api() { gh_get "$1" application/vnd.github+json; }

# api_raw PATH: GET a contents path as the raw file.
api_raw() { gh_get "$1" application/vnd.github.raw+json; }

# api_list PATH: every page of a list endpoint as one JSON array. The explicit
# returns matter: called from an `if`, errexit is off and a failed page would
# otherwise pass for the last one.
api_list() {
  local sep='?' page chunk n pages
  if [[ $1 == *\?* ]]; then sep='&'; fi
  pages=$(mktemp "$WORK/pages.XXXXXX")
  for ((page = 1; page <= 10; page++)); do
    chunk=$(api "$1${sep}per_page=100&page=$page") || return 1
    printf '%s\n' "$chunk" >>"$pages"
    n=$(jq length <<<"$chunk") || return 1
    if ((n < 100)); then break; fi
  done
  jq -s 'add // []' "$pages"
}

# api_write METHOD PATH JSON. No curl retry: a POST that timed out may still
# have created the issue or comment. The next run picks up anything missed.
api_write() {
  fetch "$1 $2" -X "$1" -H 'Accept: application/vnd.github+json' -H 'X-GitHub-Api-Version: 2022-11-28' \
    -H 'User-Agent: bwn-sentinel' "${AUTH[@]}" -H 'Content-Type: application/json' \
    --data-binary @- "$API/$2" <<<"$3" >/dev/null
}

# act DESCRIPTION METHOD PATH JSON: a write, or its description under --dry-run.
# A failed write is logged and returns 1; the caller skips what depended on it.
act() {
  local desc=$1 kind
  shift
  if ((DRY_RUN)); then
    log "  [dry-run] would $desc" | tee -a "$WORK/actions"
    return 0
  fi
  log "  $desc" | tee -a "$WORK/actions"
  if api_write "$@" 2>/dev/null; then return 0; fi
  kind=$(error_kind)
  log "  FAILED ($kind): $desc: $(tail -n1 "$WORK/neterr" | cut -f2-)" | tee -a "$WORK/actions"
  printf 'ERROR\tissues\t-\t%s: %s\t-\n' "$kind" "could not $desc" >>"$WORK/results"
  if [[ $kind != transient ]]; then BUGS=$((BUGS + 1)); fi
  return 1
}

# ------------------------------------------------------------------ helpers

to_epoch() {
  jq -rn --arg d "$1" '$d | sub("\\.[0-9]+Z$"; "Z")
    | if test("^[0-9]{4}-[0-9]{2}-[0-9]{2}$") then . + "T00:00:00Z" else . end | fromdate'
}
iso() { jq -rn --argjson t "$1" '$t | todate | sub(":[0-9]{2}Z$"; "Z")'; }

# main_head: "<sha> <commit epoch>" for the tip of $BRANCH, fetched once; the
# commit message goes to $WORK/head-msg.
main_head() {
  if [[ ! -s $WORK/head ]]; then
    api "repos/$REPO/commits/${SENTINEL_HEAD:-$BRANCH}" >"$WORK/commit.json"
    jq -r '.commit.message // ""' "$WORK/commit.json" >"$WORK/head-msg"
    jq -r '"\(.sha) \(.commit.committer.date | fromdate)"' "$WORK/commit.json" >"$WORK/head.part"
    mv "$WORK/head.part" "$WORK/head"
  fi
  cat "$WORK/head"
}

# skips_ci: main's head commit asks GitHub not to run push workflows.
# publish.yml's manual version bump commits "chore(release): vX [skip ci]".
skips_ci() { grep -qiE '\[(skip ci|ci skip|no ci|skip actions|actions skip)\]' "$WORK/head-msg"; }

# npm_doc: dist-tags and publish times from the registry, fetched once.
npm_doc() {
  if [[ ! -s $WORK/npm.json ]]; then
    fetch "GET npm $NPM_PKG" --retry 1 --retry-delay 2 -H 'Accept: application/json' "$NPM_REGISTRY/$NPM_PKG" |
      jq -c '{"dist-tags": (."dist-tags" // {}), time: (.time // {})}' >"$WORK/npm.part"
    mv "$WORK/npm.part" "$WORK/npm.json"
  fi
  cat "$WORK/npm.json"
}
npm_tag() { npm_doc | jq -r --arg t "$1" '."dist-tags"[$t] // empty'; }

# runs_until_now: drop workflow runs created after $NOW (only matters for backtests).
runs_until_now() { jq -c --argjson now "$NOW" '{workflow_runs: [.workflow_runs[] | select((.created_at | fromdate) <= $now)]}'; }

# Results: one line per outcome, "STATUS<TAB>CHECK<TAB>SIGNATURE<TAB>TEXT<TAB>DETAILS".
# A FAIL's markdown details come on stdin and go to the DETAILS file.
pass() { printf 'PASS\t%s\t-\t%s\t-\n' "$CHECK" "$1" >>"$WORK/results"; }
skip() { printf 'SKIP\t%s\t-\t%s\t-\n' "$CHECK" "$1" >>"$WORK/results"; }
fail() {
  local d
  d=$(mktemp "$WORK/details.XXXXXX")
  cat >"$d"
  printf 'FAIL\t%s\t%s\t%s\t%s\n' "$CHECK" "$1" "$2" "$d" >>"$WORK/results"
}
failed_here() { awk -F'\t' -v c="$CHECK" '$1 == "FAIL" && $2 == c { f = 1 } END { exit !f }' "$WORK/results"; }

# request_dispatch SIG WORKFLOW SHA: dispatch WORKFLOW on $BRANCH once for SHA.
# The record of it lives in SIG's issue, so the dispatch is not repeated.
request_dispatch() { printf '%s\t%s\t%s\t%s\n' "$CHECK" "$1" "$2" "$3" >>"$WORK/dispatch"; }

# ---------------------------------------------------------- frequent checks

# GitHub starts no workflow_run workflows from a run dispatched with the
# workflow token: "events triggered by the GITHUB_TOKEN, with the exception of
# workflow_dispatch and repository_dispatch, will not create a new workflow
# run" (docs.github.com, "Triggering a workflow"; confirmed for workflow_run
# in github.com/orgs/community/discussions/48748). So publish.yml never chains
# from a release.yml run the sentinel dispatched, and the sentinel dispatches
# publish.yml itself.
JQ_BOT='.event == "workflow_dispatch" and ((.triggering_actor.login // .actor.login) == "github-actions[bot]")'

# in_flight BOT VERSION TEXT DETAILS: a release or publish on its way. After the
# sentinel's own dispatch (BOT=1) it stays a release-gap failure, so that one
# issue, which holds the dispatch records, stays open until npm has the version.
in_flight() {
  if (($1)); then
    fail release-gap "v$2 is not on npm yet: $3" <<<"$4"
  else
    pass "$3"
  fi
}

# U22: GitHub dropped the push event for 2bb2a27 (v0.14.6); release.yml never
# ran and nothing noticed until the user asked.
check_release_gap() {
  local head sha epoch ver latest next npm_is age runs n run ok bot mins pubs pub pmins link
  head=$(main_head)
  sha=${head% *} epoch=${head#* }
  ver=$(api_raw "repos/$REPO/contents/package.json?ref=$sha" | jq -er .version)
  latest=$(npm_tag latest)
  next=$(npm_tag next)
  npm_is="npm has latest=\`${latest:-none}\` and next=\`${next:-none}\`"
  link="[\`${sha:0:7}\`]($SERVER/$REPO/commit/$sha)"
  if [[ $ver == "$latest" || $ver == "$next" ]]; then
    pass "v$ver (main ${sha:0:7}) is on npm (latest=${latest:-none}, next=${next:-none})"
    return
  fi
  age=$(((NOW - epoch) / 60))
  if ((age < RELEASE_GRACE_MIN)); then
    pass "main ${sha:0:7} (v$ver) is $age min old; release.yml has $RELEASE_GRACE_MIN min to start"
    return
  fi
  runs=$(api "repos/$REPO/actions/workflows/release.yml/runs?head_sha=$sha&per_page=20" | runs_until_now)
  n=$(jq '.workflow_runs | length' <<<"$runs")
  if ((n == 0)); then
    request_dispatch release-gap release.yml "$sha"
    fail release-gap "v$ver is on main but release.yml never ran for it" <<EOF
main is at $link, committed $(iso "$epoch") ($age min ago), with package.json version **$ver**.
$npm_is, and release.yml has no run for this commit.

GitHub dropped a push event like this once before (U22: 2bb2a27, v0.14.6). The sentinel dispatches release.yml on $BRANCH once per commit. publish.yml does not chain from a run dispatched with the workflow token, so when that release succeeds the sentinel dispatches publish.yml too (version_bump none). This issue stays open until npm has v$ver.
EOF
    return
  fi

  run=$(jq -c 'first(.workflow_runs[] | select(.status != "completed")) // empty' <<<"$runs")
  if [[ -n $run ]]; then
    bot=$(jq -r "if $JQ_BOT then 1 else 0 end" <<<"$run")
    in_flight "$bot" "$ver" "release.yml is running for main ${sha:0:7} (v$ver)" \
      "$(jq -r '"The sentinel dispatched release.yml for main '"$link"' (v'"$ver"'); [run \(.run_number)](\(.html_url)) is in progress. When it succeeds, the sentinel dispatches publish.yml."' <<<"$run")"
    return
  fi
  ok=$(jq -c 'first(.workflow_runs[] | select(.conclusion == "success")) // empty' <<<"$runs")
  if [[ -z $ok ]]; then
    fail release-gap/failed "release.yml failed for v$ver" <<EOF
release.yml ran for main $link (v$ver) and no attempt succeeded. Latest: $(jq -r '.workflow_runs[0] | "[\(.event) run, attempt \(.run_attempt)](\(.html_url)) concluded **\(.conclusion)**"' <<<"$runs").
$npm_is. The sentinel does not re-dispatch a failed release.
EOF
    return
  fi

  bot=$(jq -r "if $JQ_BOT then 1 else 0 end" <<<"$ok")
  mins=$(((NOW - $(to_epoch "$(jq -r .updated_at <<<"$ok")")) / 60))
  # publish.yml runs since this release started; a skipped run is one whose
  # gate said no (a failed release, say), so it does not count.
  pubs=$(api "repos/$REPO/actions/workflows/publish.yml/runs?head_sha=$sha&per_page=20" | runs_until_now |
    jq -c --arg since "$(jq -r .created_at <<<"$ok")" '[.workflow_runs[] | select(.created_at >= $since and .conclusion != "skipped")]')
  pub=$(jq -c 'first(.[]) // empty' <<<"$pubs")
  if [[ -z $pub ]]; then
    if ((bot)); then
      request_dispatch release-gap publish.yml "$sha"
      fail release-gap "v$ver is not on npm yet: release.yml succeeded, publish.yml has not run" <<EOF
The sentinel dispatched release.yml for main $link (v$ver), and [that run]($(jq -r .html_url <<<"$ok")) succeeded $mins min ago. GitHub starts no workflow_run workflows from a run dispatched with the workflow token, so publish.yml cannot chain from it: the sentinel dispatches publish.yml on $BRANCH (version_bump none) once for this commit.
$npm_is.
EOF
      return
    fi
    if ((mins < PUBLISH_START_MIN)); then
      pass "release.yml finished for v$ver $mins min ago; publish.yml has $PUBLISH_START_MIN min to start"
      return
    fi
    request_dispatch release-gap/unpublished publish.yml "$sha"
    fail release-gap/unpublished "v$ver was released but publish.yml never started" <<EOF
release.yml [succeeded]($(jq -r .html_url <<<"$ok")) for main $link (v$ver) $mins min ago, and publish.yml has no run since. publish.yml starts through a workflow_run event, which GitHub dropped (as it once dropped a push event, U22).
The sentinel dispatches publish.yml on $BRANCH (version_bump none) once for this commit. $npm_is.
EOF
    return
  fi
  if [[ $(jq -r .status <<<"$pub") != completed ]]; then
    in_flight "$bot" "$ver" "publish.yml is running for v$ver" \
      "$(jq -r '"release.yml succeeded for main '"$link"' (v'"$ver"'); publish.yml [run \(.run_number)](\(.html_url)) is in progress."' <<<"$pub")"
    return
  fi
  if [[ $(jq -r .conclusion <<<"$pub") == success ]]; then
    pmins=$(((NOW - $(to_epoch "$(jq -r .updated_at <<<"$pub")")) / 60))
    if ((pmins < PUBLISH_GRACE_MIN)); then
      in_flight "$bot" "$ver" "publish.yml finished for v$ver $pmins min ago; npm has $PUBLISH_GRACE_MIN min to show it" \
        "publish.yml [run $(jq -r .run_number <<<"$pub")]($(jq -r .html_url <<<"$pub")) succeeded $pmins min ago. $npm_is."
      return
    fi
    fail release-gap/unpublished "publish.yml succeeded for v$ver but npm does not have it" <<EOF
publish.yml [run $(jq -r .run_number <<<"$pub")]($(jq -r .html_url <<<"$pub")) for main $link succeeded $pmins min ago, yet $npm_is.
Check that run's "already on npm" and publish steps.
EOF
    return
  fi
  fail release-gap/unpublished "publish.yml failed for v$ver" <<EOF
release.yml succeeded for main $link (v$ver), then $(jq -r '"publish.yml [\(.event) run, attempt \(.run_attempt)](\(.html_url)) concluded **\(.conclusion)**"' <<<"$pub").
$npm_is. The sentinel does not re-dispatch a failed publish; re-run it once the cause is fixed.
EOF
}

# U03: main stayed red on a cargo audit failure and nobody was watching.
check_main_red() {
  local head sha epoch age n runs latest concl jobs
  head=$(main_head)
  sha=${head% *} epoch=${head#* }
  age=$(((NOW - epoch) / 60))
  # 2bb2a27 had no ci.yml push run either. A [skip ci] commit has none on purpose.
  if ((age >= CI_GRACE_MIN)) && ! skips_ci; then
    n=$(api "repos/$REPO/actions/workflows/ci.yml/runs?head_sha=$sha&per_page=20" | runs_until_now | jq '.workflow_runs | length')
    if ((n == 0)); then
      request_dispatch main-red/no-run ci.yml "$sha"
      fail main-red/no-run "ci.yml never ran for main ${sha:0:7}" <<EOF
main is at [\`${sha:0:7}\`]($SERVER/$REPO/commit/$sha), committed $(iso "$epoch") ($age min ago), and ci.yml has no run for it. GitHub probably dropped the push event (as for 2bb2a27).
The sentinel dispatches ci.yml on $BRANCH once for this commit; its result shows up here on the next runs.
EOF
    fi
  fi
  # Push and dispatched runs only: a fork's PR from its own "main" also has
  # head_branch main.
  runs=$(api "repos/$REPO/actions/workflows/ci.yml/runs?branch=$BRANCH&status=completed&per_page=20" | runs_until_now)
  # Cancelled runs are ones that cancel-in-progress replaced with a newer push.
  latest=$(jq -c 'first(.workflow_runs[] | select(.event == "push" or .event == "workflow_dispatch")
    | select(.conclusion != "cancelled" and .conclusion != "skipped")) // empty' <<<"$runs")
  if [[ -z $latest ]]; then
    if ! failed_here; then skip "no completed ci.yml push run on $BRANCH"; fi
    return
  fi
  concl=$(jq -r .conclusion <<<"$latest")
  if [[ $concl == success ]]; then
    if ! failed_here; then
      pass "$(jq -r '"ci.yml run \(.run_number) on \(.head_sha[:7]): success"' <<<"$latest")"
    fi
    return
  fi
  jobs=$(api "repos/$REPO/actions/runs/$(jq -r .id <<<"$latest")/jobs?filter=latest&per_page=100" |
    jq -r '.jobs[] | select(.conclusion != "success" and .conclusion != "skipped" and .conclusion != null)
      | "- [\(.name)](\(.html_url)): \(.conclusion)"')
  fail main-red "CI is red on $BRANCH" <<EOF
$(jq -r '"The latest completed ci.yml run on main, [run \(.run_number)](\(.html_url)) for `\(.head_sha[:7])` (attempt \(.run_attempt)), concluded **\(.conclusion)**."' <<<"$latest")

${jobs:-No failing job was listed.}
EOF
}

# U05: the site was not updated after a release until the user asked.
check_site_version() {
  local html sv badge latest pub mins stale=""
  if ! html=$(curl -fsSL --retry 2 --retry-delay 2 --max-time 30 -A 'bwn-sentinel' "$SITE_URL"); then
    skip "could not fetch $SITE_URL"
    return
  fi
  sv=$(grep -o '"softwareVersion"[[:space:]]*:[[:space:]]*"[^"]*"' <<<"$html" | head -n1 | sed -E 's/.*"([^"]*)"$/\1/' || true)
  badge=$(grep -o 'class="ver"[^>]*>v[^<]*' <<<"$html" | head -n1 | sed -E 's/.*>v//' || true)
  latest=$(npm_tag latest)
  if [[ -z $latest ]]; then
    skip "npm has no latest tag for $NPM_PKG"
    return
  fi
  if [[ -z $sv && -z $badge ]]; then
    fail site-version/unreadable "$SITE_URL shows no version the sentinel can read" <<EOF
The sentinel reads the JSON-LD \`"softwareVersion"\` and the header badge (\`<span class="ver">vX.Y.Z</span>\`) on $SITE_URL and found neither. If the markup changed on purpose, update check_site_version in scripts/field/sentinel.sh.
EOF
    return
  fi
  # The docs can go out before release.yml and publish.yml finish (v0.14.0's
  # did, by 7 minutes). A site ahead of npm latest is fine while it shows the
  # version on main or on npm next; release-gap watches that release.
  local ok=" $latest " next head ver
  if [[ (-n $sv && $sv != "$latest") || (-n $badge && $badge != "$latest") ]]; then
    next=$(npm_tag next)
    head=$(main_head)
    ver=$(api_raw "repos/$REPO/contents/package.json?ref=${head%% *}" | jq -er .version)
    ok+="$next $ver "
  fi
  if [[ -n $sv && $ok != *" $sv "* ]]; then stale+="- JSON-LD \`softwareVersion\`: **$sv**"$'\n'; fi
  if [[ -n $badge && $ok != *" $badge "* ]]; then stale+="- header badge: **v$badge**"$'\n'; fi
  if [[ -z $stale ]]; then
    if [[ ${sv:-$latest} != "$latest" || ${badge:-$latest} != "$latest" ]]; then
      pass "$SITE_URL shows v${sv:-$badge}, ahead of npm latest ($latest): the version on main or npm next, so a release is on its way"
      return
    fi
    pass "$SITE_URL shows v$latest (${sv:+JSON-LD}${sv:+${badge:+ and }}${badge:+badge}), same as npm latest"
    return
  fi
  pub=$(npm_doc | jq -r --arg v "$latest" '.time[$v] // empty')
  if [[ -n $pub ]]; then
    mins=$(((NOW - $(to_epoch "$pub")) / 60))
    if ((mins < SITE_GRACE_MIN)); then
      pass "site lags npm v$latest, which was published $mins min ago (grace $SITE_GRACE_MIN min)"
      return
    fi
  fi
  fail site-version "$SITE_URL does not show npm latest ($latest)" <<EOF
npm latest is **$latest**${pub:+, published $pub}. $SITE_URL shows:
${stale}
Update the version on the docs site (buildwithnexus-docs) and anything else the release changed.
EOF
}

# ------------------------------------------------------------- daily checks

# U03: RUSTSEC-2026-0285 in rustls kept main red and forced 0.14.1.
check_audit() {
  local out n warn
  if ! command -v cargo-audit >/dev/null 2>&1; then
    # sentinel.yml passes the install step's outcome: a failed download is
    # transient, a missing binary after a good install is a sentinel bug.
    if [[ ${SENTINEL_AUDIT_INSTALL:-} == failure ]]; then
      net_error transient "installing cargo-audit failed (see the Install cargo-audit step)"
      return 1
    fi
    if [[ -n ${CI:-} ]]; then
      echo "cargo-audit is not installed" >&2
      return 1
    fi
    skip "cargo-audit is not installed (cargo install cargo-audit --locked)"
    return
  fi
  if [[ ! -f $ROOT/Cargo.lock ]]; then
    skip "no Cargo.lock in $ROOT"
    return
  fi
  # Exit status 1 only means vulnerabilities were found; the JSON decides.
  out=$(cd "$ROOT" && cargo audit -f Cargo.lock --json 2>"$WORK/audit.err") || true
  if ! jq -e '.vulnerabilities.list' >/dev/null 2>&1 <<<"$out"; then
    cat "$WORK/audit.err" >&2
    # cargo audit clones the advisory database from GitHub first.
    if [[ -z $out ]] && grep -qiE 'fetch|network|connect|timed? ?out|resolve|tls|ssl|http' "$WORK/audit.err"; then
      net_error transient "cargo audit could not fetch the advisory database: $(tail -n1 "$WORK/audit.err")"
    else
      echo "cargo audit produced no JSON report" >&2
    fi
    return 1
  fi
  n=$(jq '.vulnerabilities.list | length' <<<"$out")
  warn=$(jq -r '[.warnings // {} | to_entries[] | select(.value | length > 0) | "\(.value | length) \(.key)"] | join(", ")' <<<"$out")
  if ((n == 0)); then
    pass "no vulnerabilities in Cargo.lock (warnings: ${warn:-none})"
    return
  fi
  local id pkgs title url patched
  while IFS=$'\t' read -r id pkgs title url patched; do
    fail "audit/$id" "$id in $pkgs: $title" <<EOF
\`cargo audit -f Cargo.lock\` on $BRANCH reports **[$id]($url)** in \`$pkgs\`: $title

Patched versions: $patched

ci.yml's audit job fails every push to main until this is fixed (U03). Usually \`cargo update -p <crate> --precise <patched>\` in a PR is enough.
EOF
  done < <(jq -r '.vulnerabilities.list | group_by(.advisory.id)[]
    | [.[0].advisory.id,
       ([.[] | "\(.package.name) \(.package.version)"] | unique | join(", ")),
       .[0].advisory.title,
       (.[0].advisory.url // "https://rustsec.org/advisories/\(.[0].advisory.id)"),
       ((.[0].versions.patched // []) | if length == 0 then "none" else join(", ") end)]
    | @tsv' <<<"$out")
}

# Entries of every `labels:` list in dependabot.yml, block or [flow] style.
dependabot_labels() {
  awk '
    function unq(s) { gsub(/^[[:space:]"'\'']+|[[:space:]"'\'']+$/, "", s); return s }
    /^[[:space:]]*#/ { next }
    { sub(/[[:space:]]+#.*$/, "") }
    {
      match($0, /^[[:space:]]*/); ind = RLENGTH
      if (inlist) {
        if ($0 ~ /^[[:space:]]*$/) next
        if (ind >= lind && $0 ~ /^[[:space:]]*-[[:space:]]/) { s = $0; sub(/^[[:space:]]*-/, "", s); print unq(s); next }
        inlist = 0
      }
      if ($0 ~ /^[[:space:]]*labels:[[:space:]]*$/) { inlist = 1; lind = ind; next }
      if ($0 ~ /^[[:space:]]*labels:[[:space:]]*\[/) {
        s = $0; sub(/^[^[]*\[/, "", s); sub(/\].*$/, "", s)
        n = split(s, a, ","); for (i = 1; i <= n; i++) if (unq(a[i]) != "") print unq(a[i])
      }
    }' "$1"
}

# U01: Dependabot PR #81 wanted labels the repo did not have.
check_labels() {
  local f=$ROOT/.github/dependabot.yml lf=$ROOT/.github/workflows/labels.yml want have missing listed="" fix="" head
  if [[ ! -f $f ]]; then
    skip "no .github/dependabot.yml"
    return
  fi
  want=$(dependabot_labels "$f" | tr '[:upper:]' '[:lower:]' | sort -u)
  if [[ -z $want ]]; then
    pass "dependabot.yml asks for no labels"
    return
  fi
  # Label names are case-insensitive on GitHub.
  have=$(api_list "repos/$REPO/labels" | jq -r '.[].name | ascii_downcase' | sort -u)
  missing=$(comm -23 <(printf '%s\n' "$want") <(printf '%s\n' "$have"))
  if [[ -z $missing ]]; then
    pass "all $(wc -l <<<"$want") labels dependabot.yml asks for exist ($(paste -sd, - <<<"$want" | sed 's/,/, /g'))"
    return
  fi
  # labels.yml creates every label it lists (`create NAME COLOR ...`), so for
  # those nobody has to run it: the sentinel dispatches it once per commit.
  if [[ -f $lf ]]; then
    listed=$(comm -12 <(printf '%s\n' "$missing") <(awk '$1 == "create" && NF > 1 { print tolower($2) }' "$lf" | tr -d "\"'" | sort -u))
  fi
  if [[ -n $listed ]]; then
    head=$(main_head)
    request_dispatch labels labels.yml "${head%% *}"
    fix="The sentinel dispatches .github/workflows/labels.yml on $BRANCH once for this commit, which creates **$(paste -sd, - <<<"$listed" | sed 's/,/, /g')**."
  fi
  local unlisted
  unlisted=$(comm -23 <(printf '%s\n' "$missing") <(printf '%s\n' "$listed") | paste -sd, - | sed 's/,/, /g')
  if [[ -n $unlisted ]]; then
    fix+="${fix:+ }Add **$unlisted** to .github/workflows/labels.yml; once that is on $BRANCH, the sentinel dispatches it."
  fi
  missing=$(paste -sd, - <<<"$missing" | sed 's/,/, /g')
  fail labels "Dependabot labels missing: $missing" <<EOF
.github/dependabot.yml applies labels that the repository does not have: **$missing**. Dependabot then comments on every PR it opens (U01, PR #81).

$fix
EOF
}

# U01: nobody looked at open bot PRs or their failing checks.
check_stale_bot_prs() {
  local prs total rows="" num title url created sha age why why_text checks statuses
  prs=$(api_list "repos/$REPO/pulls?state=open" |
    jq -c '[.[] | select(.user.login == "dependabot[bot]") | {number, title, html_url, created_at, sha: .head.sha}]')
  total=$(jq length <<<"$prs")
  while IFS=$'\t' read -r -u 3 num title url created sha; do
    why=()
    age=$((NOW - $(to_epoch "$created")))
    if ((age > BOT_PR_MAX_DAYS * 86400)); then why+=("open $((age / 86400)) days"); fi
    checks=$(api "repos/$REPO/commits/$sha/check-runs?per_page=100" |
      jq -r '[.check_runs[] | select(.conclusion == "failure" or .conclusion == "timed_out"
        or .conclusion == "startup_failure" or .conclusion == "action_required") | .name] | unique | join(", ")')
    statuses=$(api "repos/$REPO/commits/$sha/status" |
      jq -r '[.statuses[] | select(.state == "failure" or .state == "error") | .context] | unique | join(", ")')
    if [[ -n $checks ]]; then why+=("failing checks: $checks"); fi
    if [[ -n $statuses ]]; then why+=("failing statuses: $statuses"); fi
    if ((${#why[@]})); then
      printf -v why_text '%s; ' "${why[@]}"
      rows+="| [#$num]($url) | ${title//|/\\|} | ${why_text%; } |"$'\n'
    fi
  done 3< <(jq -r '.[] | [.number, .title, .html_url, .created_at, .sha] | @tsv' <<<"$prs")
  if [[ -z $rows ]]; then
    pass "$total open Dependabot PRs, none older than $BOT_PR_MAX_DAYS days or failing"
    return
  fi
  fail stale-bot-prs "$(grep -c . <<<"$rows") Dependabot PRs need attention" <<EOF
Open Dependabot PRs that are older than $BOT_PR_MAX_DAYS days or have failing checks:

| PR | Title | Problem |
|---|---|---|
${rows}
Merge, fix or close them; a bot PR left red hides the next real failure (U01, U02).
EOF
}

# workflow_hits LABEL FILE...: "path:line: text" for each line using LABEL as a
# whole token outside comments (runs-on values and matrix entries).
workflow_hits() {
  awk -v L="$1" '
    {
      line = $0; sub(/(^|[[:space:]])#.*$/, "", line)
      n = split(line, t, /[][[:space:],:"'\''{}()]+/)
      for (i = 1; i <= n; i++) if (t[i] == L) {
        f = FILENAME; sub(/^.*\/\.github\//, ".github/", f)
        s = $0; gsub(/^[[:space:]]+|[[:space:]]+$/, "", s)
        print f ":" FNR ": `" s "`"; break
      }
    }' "${@:2}"
}

check_runner_labels() {
  local tsv=$ROOT/field/deprecated-labels.tsv label date note hits days when entries=0 flagged=0
  local wf=("$ROOT"/.github/workflows/*.yml "$ROOT"/.github/workflows/*.yaml)
  local files=()
  for f in "${wf[@]}"; do if [[ -f $f ]]; then files+=("$f"); fi; done
  if [[ ! -f $tsv ]]; then
    skip "no field/deprecated-labels.tsv"
    return
  fi
  if ((${#files[@]} == 0)); then
    skip "no workflow files"
    return
  fi
  while IFS=$'\t' read -r label date note; do
    if [[ -z $label || $label == \#* ]]; then continue; fi
    entries=$((entries + 1))
    hits=$(workflow_hits "$label" "${files[@]}")
    if [[ -z $hits ]]; then continue; fi
    flagged=$((flagged + 1))
    days=$((($(to_epoch "$date") - NOW) / 86400))
    if ((days > 0)); then when="$date, **$days days from now**"; else when="$date, $((-days)) days ago"; fi
    fail "runner-labels/$label" "Workflows use runner label $label ($date)" <<EOF
\`$label\` is listed in field/deprecated-labels.tsv ($when):

$note

Used in:
- ${hits//$'\n'/$'\n'- }
EOF
  done <"$tsv"
  if ((flagged == 0)); then
    pass "none of the $entries labels in deprecated-labels.tsv appear in ${#files[@]} workflows"
  fi
}

# ---------------------------------------------------------------- run checks

log "sentinel: $REPO | checks: $SELECTED | $( ((DRY_RUN)) && echo dry-run || echo live) | via $VIA"
for c in $SELECTED; do
  CHECK=$c
  for attempt in 1 2; do
    : >"$WORK/neterr"
    set +e
    (
      set -e
      "check_${c//-/_}"
    ) 2>"$WORK/stderr"
    rc=$?
    set -e
    if [[ -s $WORK/stderr ]]; then sed "s/^/  [$c] /" "$WORK/stderr" >&2; fi
    if ((rc == 0)); then break; fi
    # A check that died part-way must not count as a pass, a fresh failure or
    # a dispatch request.
    awk -F'\t' -v c="$c" '$2 != c' "$WORK/results" >"$WORK/tmp"
    mv "$WORK/tmp" "$WORK/results"
    awk -F'\t' -v c="$c" '$1 != c' "$WORK/dispatch" >"$WORK/tmp"
    mv "$WORK/tmp" "$WORK/dispatch"
    kind=$(error_kind)
    if [[ $kind == transient && $attempt == 1 ]]; then
      log "RETRY $c in ${RETRY_DELAY}s: $(tail -n1 "$WORK/neterr" | cut -f2-)"
      sleep "$RETRY_DELAY"
      continue
    fi
    if [[ $kind == transient ]]; then
      reason="transient, retried once: $(tail -n1 "$WORK/neterr" | cut -f2-)"
      echo "::warning::sentinel: $c: $reason"
    else
      reason="sentinel bug: exit $rc: $(if [[ -s $WORK/neterr ]]; then tail -n1 "$WORK/neterr" | cut -f2-; else tail -n1 "$WORK/stderr"; fi)"
      echo "::error::sentinel: $c: $reason"
      BUGS=$((BUGS + 1))
    fi
    printf 'ERROR\t%s\t-\t%s\t-\n' "$c" "$(tr '\t' ' ' <<<"$reason")" >>"$WORK/results"
    break
  done
done

while IFS=$'\t' read -r status check sig text d; do
  if [[ $status == FAIL ]]; then
    printf '%-5s %-14s %s: %s\n' "$status" "$check" "$sig" "$text"
    sed 's/^/        /' "$d"
  else
    printf '%-5s %-14s %s\n' "$status" "$check" "$text"
  fi
done <"$WORK/results"

# ------------------------------------------------------------------- issues

ISSUES='[]'
# Marker on the first line of every sentinel issue body; it holds the state.
JQ_META='def meta: (capture("<!-- sentinel (?<m>[^\n]*?) -->") // null)
  | if . then [.m | scan("(\\w+)=\"([^\"]*)\"") | {(.[0]): .[1]}] | add else {} end;'

render_body() { # check sig streak first dispatched status details
  printf '<!-- sentinel check="%s" sig="%s" streak="%s" first="%s" dispatched="%s" -->\n' "$1" "$2" "$3" "$4" "$5"
  # shellcheck disable=SC2016 # the backticks are markdown
  printf '**Check:** `%s`, **signature:** `%s`, **first seen:** %s\n\n' "$1" "$2" "$4"
  printf '**Status:** %s\n\n' "$6"
  printf '<!-- sentinel:details -->\n%s\n<!-- /sentinel:details -->\n\n' "$7"
  printf -- '---\n<sub>Kept by [sentinel.yml](%s) (scripts/field/sentinel.sh), which closes this issue after the check passes %s runs in a row. Close it as "not planned" to mute this signature. Keep the first line of this description: it holds the sentinel'"'"'s state.</sub>\n' \
    "$SERVER/$REPO/blob/$BRANCH/.github/workflows/sentinel.yml" "$CLOSE_AFTER"
}

old_details() { jq -r '.body | capture("<!-- sentinel:details -->\n(?<d>[\\s\\S]*)\n<!-- /sentinel:details -->").d // "(details not kept)"' <<<"$1"; }

LABEL_READY=0
ensure_label() {
  local names
  if ((LABEL_READY)); then return 0; fi
  # If the list fails, try again for the next new issue; the issue itself is
  # still opened.
  names=$(api_list "repos/$REPO/labels" 2>/dev/null) || return 0
  LABEL_READY=1
  if jq -e --arg l "$LABEL" 'any(.[]; .name | ascii_downcase == $l)' >/dev/null <<<"$names"; then
    return 0
  fi
  act "create the \"$LABEL\" label" POST "repos/$REPO/labels" \
    "$(jq -nc --arg n "$LABEL" '{name: $n, color: "b60205", description: "Opened by the scheduled sentinel workflow"}')" || :
}

# dispatch_workflow WORKFLOW: run it on $BRANCH, as `gh workflow run WORKFLOW --ref $BRANCH`.
dispatch_workflow() {
  local body
  body=$(jq -nc --arg r "$BRANCH" '{ref: $r}')
  # Publish what is already on main; a bump from here could not be tagged.
  if [[ $1 == publish.yml ]]; then body=$(jq -c '. + {inputs: {version_bump: "none"}}' <<<"$body"); fi
  act "dispatch $1 on $BRANCH" POST "repos/$REPO/actions/workflows/$1/dispatches" "$body"
}

comment() { # number text
  act "comment on #$1: $2" POST "repos/$REPO/issues/$1/comments" "$(jq -nc --arg b "$2 ($RUN_LINK)" '{body: $b}')" || :
}

# dispatch_text RECORDS: the marker's "WORKFLOW:SHA@TIME ..." as a sentence.
dispatch_text() {
  local e rest out=""
  for e in $1; do
    rest=${e#*:}
    out+="${e%%:*} for \`${rest:0:7}\` at ${rest#*@}, "
  done
  if [[ -n $out ]]; then printf ' The sentinel dispatched %s.' "${out%, }"; fi
}

handle_fail() { # check sig title detailsfile
  local check=$1 sig=$2 title="sentinel: $3" details open closed num first disp="" status prev wf dsha fresh=()
  details=$(cat "$4")
  open=$(jq -c --arg s "$sig" '[.[] | select(.sig == $s and .state == "open")] | max_by(.number) // empty' <<<"$ISSUES")
  closed=$(jq -c --arg s "$sig" '[.[] | select(.sig == $s and .state == "closed")] | max_by(.number) // empty' <<<"$ISSUES")
  prev=${open:-$closed}
  if [[ -n $prev ]]; then disp=$(jq -r '.dispatched // ""' <<<"$prev"); fi

  # Dispatch each requested workflow once per commit, even if a human muted
  # the issue: the dispatch closes the gap, the issue only reports it. A
  # dispatch that failed is not recorded, so the next run tries again.
  while IFS=$'\t' read -r -u 4 _ dsig wf dsha; do
    if [[ $dsig != "$sig" ]]; then continue; fi
    if [[ " $disp " == *" $wf:$dsha@"* ]]; then
      log "  $wf was already dispatched for ${dsha:0:7}; not again"
    elif dispatch_workflow "$wf"; then
      disp="${disp:+$disp }$wf:$dsha@$(iso "$NOW")"
      fresh+=("$wf")
    fi
  done 4<"$WORK/dispatch"
  status="failing, last seen $(iso "$NOW") in $RUN_LINK. Pass streak 0 of $CLOSE_AFTER.$(dispatch_text "$disp")"

  if [[ -n $open ]]; then
    num=$(jq -r .number <<<"$open")
    first=$(jq -r '.first // ""' <<<"$open")
    act "update #$num ($sig still failing)" PATCH "repos/$REPO/issues/$num" \
      "$(jq -nc --arg t "$title" --arg b "$(render_body "$check" "$sig" 0 "${first:-$(iso "$NOW")}" "$disp" "$status" "$details")" '{title: $t, body: $b}')" || return 0
    if (($(jq -r '.streak // 0' <<<"$open") > 0)); then comment "$num" "Failing again, so the pass streak is back to 0."; fi
    if ((${#fresh[@]})); then comment "$num" "Dispatched ${fresh[*]} on $BRANCH."; fi
    return 0
  fi
  if [[ -n $closed ]]; then
    num=$(jq -r .number <<<"$closed")
    if [[ $(jq -r '.state_reason // ""' <<<"$closed") == not_planned ]]; then
      log "  muted: #$num ($sig) was closed as not planned; leaving it closed"
      if ((${#fresh[@]})); then
        act "record the dispatch on closed #$num" PATCH "repos/$REPO/issues/$num" \
          "$(jq -nc --arg b "$(render_body "$check" "$sig" 0 "$(jq -r '.first // ""' <<<"$closed")" "$disp" "muted (closed as not planned). $status" "$details")" '{body: $b}')" || :
      fi
      return 0
    fi
    act "reopen #$num ($sig failing again)" PATCH "repos/$REPO/issues/$num" \
      "$(jq -nc --arg t "$title" --arg b "$(render_body "$check" "$sig" 0 "$(iso "$NOW")" "$disp" "$status" "$details")" '{title: $t, body: $b, state: "open"}')" || return 0
    comment "$num" "Failing again, so the sentinel reopened this issue."
    return 0
  fi
  ensure_label
  act "open issue \"$title\"" POST "repos/$REPO/issues" \
    "$(jq -nc --arg t "$title" --arg l "$LABEL" --arg b "$(render_body "$check" "$sig" 0 "$(iso "$NOW")" "$disp" "$status" "$details")" '{title: $t, body: $b, labels: [$l]}')" || :
}

handle_pass() { # issue-json
  local num sig check streak details status
  num=$(jq -r .number <<<"$1")
  sig=$(jq -r .sig <<<"$1")
  check=$(jq -r .check <<<"$1")
  streak=$(($(jq -r '.streak // 0' <<<"$1") + 1))
  details=$(old_details "$1")
  if ((streak >= CLOSE_AFTER)); then
    status="passed $streak runs in a row; closed $(iso "$NOW")."
    act "close #$num ($sig passed $streak runs in a row)" PATCH "repos/$REPO/issues/$num" \
      "$(jq -nc --arg b "$(render_body "$check" "$sig" "$streak" "$(jq -r '.first // ""' <<<"$1")" "$(jq -r '.dispatched // ""' <<<"$1")" "$status" "$details")" \
        '{body: $b, state: "closed", state_reason: "completed"}')" || return 0
    comment "$num" "\`$check\` passed $streak runs in a row, so the sentinel closed this issue."
  else
    status="passing, $streak of $CLOSE_AFTER runs in a row (last $(iso "$NOW") in $RUN_LINK)."
    act "update #$num ($sig pass streak $streak of $CLOSE_AFTER)" PATCH "repos/$REPO/issues/$num" \
      "$(jq -nc --arg b "$(render_body "$check" "$sig" "$streak" "$(jq -r '.first // ""' <<<"$1")" "$(jq -r '.dispatched // ""' <<<"$1")" "$status" "$details")" '{body: $b}')" || :
  fi
}

evaluated=$(awk -F'\t' '$1 == "PASS" || $1 == "FAIL" { print $2 }' "$WORK/results" | sort -u)
log "issues:"
if [[ -n $evaluated ]]; then
  : >"$WORK/neterr"
  if raw=$(api_list "repos/$REPO/issues?labels=$LABEL&state=all" 2>/dev/null) &&
    ISSUES=$(jq -c "$JQ_META"' [.[] | select(.pull_request | not) | {number, state, state_reason, body: (.body // "")} + ((.body // "") | meta) | select(.sig)]' <<<"$raw"); then
    while IFS=$'\t' read -r -u 3 status check sig text d; do
      if [[ $status == FAIL ]]; then handle_fail "$check" "$sig" "$text" "$d"; fi
    done 3< <(grep '^FAIL' "$WORK/results" || true)
    for check in $evaluated; do
      failing=$(awk -F'\t' -v c="$check" '$1 == "FAIL" && $2 == c { print $3 }' "$WORK/results" | jq -Rsc 'split("\n") | map(select(. != ""))')
      while IFS= read -r -u 3 issue; do
        if [[ -n $issue ]]; then handle_pass "$issue"; fi
      done 3< <(jq -c --arg c "$check" --argjson f "$failing" '.[] | select(.state == "open" and .check == $c and ((.sig | IN($f[])) | not))' <<<"$ISSUES")
    done
  else
    # Without the issue list a FAIL could open a duplicate, so change nothing.
    kind=$(error_kind)
    reason="could not list sentinel issues, so no issue changes this run: $(tail -n1 "$WORK/neterr" | cut -f2-)"
    log "  $reason"
    printf 'ERROR\tissues\t-\t%s: %s\t-\n' "$kind" "$reason" >>"$WORK/results"
    if [[ $kind != transient ]]; then BUGS=$((BUGS + 1)); fi
  fi
fi
if [[ ! -s $WORK/actions ]]; then log "  no changes"; fi

if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
  {
    echo "### Sentinel ($( ((DRY_RUN)) && echo dry run || echo live))"
    echo
    echo "| Status | Check | Result |"
    echo "|---|---|---|"
    awk -F'\t' '{ t = ($3 == "-" ? $4 : $3 ": " $4); gsub(/\|/, "\\|", t); print "| " $1 " | " $2 " | " t " |" }' "$WORK/results"
    if [[ -s $WORK/actions ]]; then
      echo
      echo "Issue and dispatch actions:"
      echo
      sed 's/^ *\(.*\)$/- \1/' "$WORK/actions"
    fi
    echo
    echo "FAIL rows open issues and never fail this run. A transient ERROR (network, timeout, HTTP 408/429/5xx) was retried once and does not fail it either; only a sentinel bug does."
  } >>"$GITHUB_STEP_SUMMARY"
fi

# Only a broken sentinel turns the run red (and emails the maintainer).
if ((BUGS)); then
  echo "::error::sentinel: $BUGS error(s) that point at a bug in the sentinel or sentinel.yml; see the ERROR rows"
  exit 1
fi
