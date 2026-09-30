#!/usr/bin/env bash
# Rehearses the command blocks we hand the maintainer: each block runs in a
# fresh login shell, as the kind of user they will be, and the report gives
# exit code, duration, stdout/stderr and the "# expect:" check per block.
#
# Used on Linux and macOS by .github/workflows/field-adhoc.yml, and locally
# (for example in a docker image matching the user's distro) by the bwn-field
# skill. Windows uses run-blocks.ps1.
#
#   BLOCKS_FILE=blocks.txt scripts/field/run-blocks.sh
#   BLOCKS_B64=$(base64 -w0 blocks.txt) scripts/field/run-blocks.sh
#   scripts/field/run-blocks.sh --extract draft.md [bash|powershell] > blocks.txt
#
# Environment:
#   FIELD_SHELL     bash (default) | pwsh
#   FIELD_ELEVATED  false (default): blocks run as a non-root user (the
#                   current one, or "fieldstd" when this runs as root);
#                   true: blocks run as root
#   FIELD_CELL      label for the report (default: this OS)
#   FIELD_SETUP     shell command run once as root before the blocks, to
#                   shape a container into the cell (reported, not timed);
#                   FIELD_USER in its environment names the block user
#   FIELD_OUT       output directory (default ./field-out)
#   BLOCK_TIMEOUT   seconds per block (default 1200)
#
# Blocks are separated by a line that reads "### block". A block passes when
# it exits 0 (or the code named by "# expect-exit: N|any") and its combined
# stdout+stderr contains the text of every "# expect: TEXT" line (literal
# substring; lines that echo a failing command's source are skipped). A
# block without an "# expect:" line fails: every block the
# maintainer gets has to end in a check. Blocks run in order and stop at the
# first failure, as a person following them would.

set -uo pipefail # no -e: a failing block is a result, not a script error

die() {
  printf 'run-blocks: %s\n' "$*" >&2
  exit 2
}

# Prints the shell fences of a markdown draft as a blocks file, so what is
# rehearsed is byte-for-byte what is sent. Handles fences indented in lists.
extract() {
  local want=${2:-all}
  [ -f "$1" ] || die "no such file: $1"
  awk -v want="$want" '
    function lang_ok(l) {
      if (want == "bash") return l ~ /^(bash|sh|shell|zsh)$/
      if (want == "powershell") return l ~ /^(powershell|pwsh|ps1|ps)$/
      return l ~ /^(bash|sh|shell|zsh|powershell|pwsh|ps1|ps)$/
    }
    !infence && /^[ \t]*```/ {
      line = $0; match(line, /^[ \t]*/); ind = RLENGTH
      lang = substr(line, ind + 1); sub(/^`+[ \t]*/, "", lang); sub(/[ \t\r]+$/, "", lang)
      infence = 1; keep = lang_ok(tolower(lang)); buf = ""; next
    }
    infence && /^[ \t]*```[ \t\r]*$/ {
      infence = 0
      if (keep) { if (count++) print "### block"; printf "%s", buf }
      next
    }
    infence {
      line = $0; sub(/\r$/, "", line); k = 0
      while (k < ind && substr(line, 1, 1) ~ /[ \t]/) { line = substr(line, 2); k++ }
      buf = buf line "\n"
    }
  ' "$1"
}

if [ "${1:-}" = "--extract" ]; then
  [ $# -ge 2 ] || die "usage: run-blocks.sh --extract draft.md [bash|powershell]"
  extract "$2" "${3:-all}"
  exit 0
fi

shell=${FIELD_SHELL:-bash}
elevated=${FIELD_ELEVATED:-false}
timeout_s=${BLOCK_TIMEOUT:-1200}
case $shell in
  bash) ext='sh' ;;
  pwsh) ext=ps1 ;;
  powershell) die "powershell is Windows PowerShell 5.1: it only runs on Windows (run-blocks.ps1)" ;;
  *) die "FIELD_SHELL must be bash or pwsh, not '$shell'" ;;
esac
case $elevated in true | false) ;; *) die "FIELD_ELEVATED must be true or false" ;; esac
shell_exe=$(command -v "$shell") || die "$shell is not installed on this machine"

out=${FIELD_OUT:-field-out}
mkdir -p "$out" || die "cannot create $out"
out=$(cd "$out" && pwd)
# Block files live where any user can read them: the checkout may not be.
run=$(mktemp -d "${TMPDIR:-/tmp}/field-run.XXXXXX") || die "mktemp failed"
chmod 755 "$run"
trap 'rm -rf "$run"' EXIT

if [ -n "${BLOCKS_FILE:-}" ]; then
  [ -f "$BLOCKS_FILE" ] || die "no such file: $BLOCKS_FILE"
  cp "$BLOCKS_FILE" "$out/blocks.txt"
elif [ -n "${BLOCKS_B64:-}" ]; then
  printf '%s' "$BLOCKS_B64" | base64 --decode >"$out/blocks.txt" 2>/dev/null ||
    die "blocks input is not valid base64"
else
  die "set BLOCKS_FILE or BLOCKS_B64"
fi

# Split into block01.sh, block02.sh, ... CRs are dropped: a terminal
# converts pasted CRLF, bash reading a file does not.
count=$(awk -v dir="$run" -v ext="$ext" '
  function flush() {
    if (nb) { k++; f = sprintf("%s/block%02d.%s", dir, k, ext); printf "%s", buf > f; close(f) }
    buf = ""; nb = 0
  }
  { sub(/\r$/, "") }
  /^### block[ \t]*$/ { flush(); next }
  { buf = buf $0 "\n"; if ($0 ~ /[^ \t]/) nb = 1 }
  END { flush(); print k + 0 }
' "$out/blocks.txt")
[ "$count" -gt 0 ] || die "no blocks found (separate blocks with a line '### block')"
chmod 644 "$run"/block*."$ext"

# Who the blocks run as. A WSL or desktop user is in the sudo group and types
# a password; a rehearsal cannot type, so fieldstd gets NOPASSWD sudo instead.
if [ "$elevated" = true ]; then
  target=root
elif [ "$(id -u)" -ne 0 ]; then
  target=$(id -un)
else
  target=fieldstd
  if ! id "$target" >/dev/null 2>&1; then
    if command -v useradd >/dev/null 2>&1; then
      useradd -m -s /bin/bash "$target" || die "useradd failed"
    elif command -v adduser >/dev/null 2>&1; then
      adduser -D -s /bin/bash "$target" || die "adduser failed"
    else
      die "cannot create a non-root user here; set FIELD_ELEVATED=true"
    fi
  fi
fi
home=$(eval "echo ~$target")

# Runs after the user exists, so it can also give that user what the real
# machine has (nvm, a Node version): FIELD_USER names it.
if [ -n "${FIELD_SETUP:-}" ]; then
  [ "$(id -u)" -eq 0 ] || die "FIELD_SETUP needs root (it is for shaping a container)"
  echo "run-blocks: cell setup: $FIELD_SETUP"
  FIELD_USER=$target sh -c "$FIELD_SETUP" >"$out/setup.log" 2>&1 || {
    cat "$out/setup.log" >&2
    die "FIELD_SETUP failed"
  }
fi
if [ "$target" = fieldstd ] && command -v sudo >/dev/null 2>&1 && [ -d /etc/sudoers.d ]; then
  echo "$target ALL=(ALL) NOPASSWD:ALL" >"/etc/sudoers.d/$target"
  chmod 440 "/etc/sudoers.d/$target"
fi

# A new terminal starts from the login environment, not from this script's:
# env -i, then a login shell rebuilds PATH from the profile files. Proxy and
# CA variables pass through because the cloud container needs them to reach
# the internet at all; they are harness plumbing, not part of the cell.
base_path=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
if [ -r /etc/environment ]; then
  p=$(sed -n 's/^PATH="\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' /etc/environment | tail -n 1)
  [ -n "$p" ] && base_path=$p
fi
fresh_env=(env -i "HOME=$home" "USER=$target" "LOGNAME=$target" "SHELL=/bin/bash"
  "TERM=xterm-256color" "LANG=${LANG:-C.UTF-8}" "PATH=$base_path")
for v in HTTPS_PROXY https_proxy HTTP_PROXY http_proxy NO_PROXY no_proxy \
  SSL_CERT_FILE NODE_EXTRA_CA_CERTS CURL_CA_BUNDLE REQUESTS_CA_BUNDLE GIT_SSL_CAINFO; do
  if [ -n "${!v:-}" ]; then fresh_env+=("$v=${!v}"); fi
done

tcmd=()
if command -v timeout >/dev/null 2>&1; then
  tcmd=(timeout -k 10 "$timeout_s")
elif command -v gtimeout >/dev/null 2>&1; then
  tcmd=(gtimeout -k 10 "$timeout_s")
fi

# Runs "$@" as $target in a fresh environment, starting in its home.
as_target() {
  # shellcheck disable=SC2016 # expanded by the inner sh, as the target user
  local inner=("${fresh_env[@]}" sh -c 'cd "$HOME" 2>/dev/null; exec "$@"' sh "$@")
  if [ "$target" = "$(id -un)" ]; then
    "${tcmd[@]+"${tcmd[@]}"}" "${inner[@]}"
  elif [ "$(id -u)" -eq 0 ] && command -v runuser >/dev/null 2>&1; then
    "${tcmd[@]+"${tcmd[@]}"}" runuser -u "$target" -- "${inner[@]}"
  elif command -v sudo >/dev/null 2>&1; then
    "${tcmd[@]+"${tcmd[@]}"}" sudo -n -H -u "$target" -- "${inner[@]}"
  else
    echo "run-blocks: cannot switch to $target (no runuser or sudo)" >&2
    return 125
  fi
}

# bash -l -i is what a new terminal tab runs (WSL starts a login shell).
block_cmd() {
  case $shell in
    bash) cmd=("$shell_exe" -l -i "$1") ;;
    pwsh) cmd=("$shell_exe" -NoLogo -NoProfile -NonInteractive -File "$1") ;;
  esac
}

now_ms() {
  if [ -n "${EPOCHREALTIME:-}" ]; then
    local t=${EPOCHREALTIME//[^0-9]/}
    echo $((t / 1000))
  else
    echo $(($(date +%s) * 1000))
  fi
}

html() { sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'; }

# Colour codes (pwsh colours its errors) would hide text from the expect
# check and clutter the report; the raw logs keep them.
esc=$(printf '\033')
plain() { sed "s/${esc}\[[0-9;?]*[A-Za-z]//g" "$@"; }

# Last 150 lines and 16 KB: the job summary is capped at 1 MiB per step.
excerpt() {
  local lines
  lines=$(wc -l <"$1" | tr -d ' ')
  if [ "$lines" -gt 150 ]; then
    echo "[last 150 of $lines lines; the full log is in the artifact]"
  fi
  tail -n 150 "$1" | tail -c 16000 | plain
}

label() { # first non-blank line of a block, for the table
  awk 'NF { sub(/^[ \t]+/, ""); print substr($0, 1, 70); exit }' "$1" | html | sed 's/|/\&#124;/g'
}

# Cell facts, from the same fresh environment the blocks get: this also
# proves the user switch works before any block depends on it.
probe=$run/probe.sh
cat >"$probe" <<'EOF'
os=$( (. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME") || (sw_vers -productName 2>/dev/null; sw_vers -productVersion 2>/dev/null) | tr '\n' ' ')
libc=$(ldd --version 2>&1 | head -n 1)
sudo_ok=no; sudo -n true 2>/dev/null && sudo_ok=yes
tools=""
for c in node npm git curl rg ffmpeg ollama docker; do
  if command -v "$c" >/dev/null 2>&1; then tools="$tools $c"; fi
done
echo "os=$os; kernel=$(uname -sr); libc=$libc; user=$(id -un) uid=$(id -u) sudo=$sudo_ok; nvm=${NVM_DIR:-none}; on PATH:${tools:- none}; cpus=$(getconf _NPROCESSORS_ONLN 2>/dev/null)"
EOF
chmod 644 "$probe"
probe_sh='sh'
command -v bash >/dev/null 2>&1 && probe_sh="bash -l -i"
# shellcheck disable=SC2086 # probe_sh is two or three words on purpose
cell_facts=$(as_target $probe_sh "$probe" </dev/null 2>&1 | grep '^os=' | tail -n 1)
[ -n "$cell_facts" ] || die "could not run a probe as $target (no runuser or sudo? install one in FIELD_SETUP, or set FIELD_ELEVATED=true)"

gh_group() { if [ "${GITHUB_ACTIONS:-}" = true ]; then echo "::group::$*"; else echo "== $*"; fi; }
gh_endgroup() { if [ "${GITHUB_ACTIONS:-}" = true ]; then echo "::endgroup::"; fi; }

cell=${FIELD_CELL:-$(uname -s)}
who="non-root user $target"
[ "$target" = root ] && who="root"
summary=$out/summary.md
results=$out/results.tsv
printf 'block\tresult\texit\tseconds\tmissing_expect\n' >"$results"
details=""
rows=""
failed=0
total_ms=0

i=0
while [ "$i" -lt "$count" ]; do
  i=$((i + 1))
  n=$(printf '%02d' "$i")
  f=$run/block$n.$ext
  so=$out/block$n.out
  se=$out/block$n.err
  if [ "$failed" -ne 0 ]; then
    rows="$rows| $i | <code>$(label "$f")</code> | | | | not run |"$'\n'
    printf '%s\tnot run\t\t\t\n' "$i" >>"$results"
    continue
  fi

  want_exit=$(sed -n 's/^[[:space:]]*#[[:space:]]*expect-exit:[[:space:]]*\([^[:space:]]*\).*/\1/p' "$f" | tail -n 1)
  want_exit=${want_exit:-0}
  expects=$(sed -n 's/^[[:space:]]*#[[:space:]]*expect:[[:space:]]*//p' "$f" | sed 's/[[:space:]]*$//')

  cp "$f" "$out/"
  block_cmd "$f"
  start=$(now_ms)
  as_target "${cmd[@]}" </dev/null >"$so" 2>"$se"
  code=$?
  ms=$(($(now_ms) - start))
  total_ms=$((total_ms + ms))
  secs=$(awk -v m="$ms" 'BEGIN { printf "%.1f", m / 1000 }')

  # bash -i without a terminal prints these two lines; they come from the
  # harness, not from the block.
  grep -v -e '^bash: cannot set terminal process group' -e '^bash: no job control in this shell$' \
    "$se" >"$se.tmp" || true
  mv "$se.tmp" "$se"

  # PowerShell repeats a failing source line in its error ("+ line" in 5.1,
  # "  2 |  line" in 7), as does bash -x, so a check line that failed would
  # still contain its own expect text. Those lines are left out of the match.
  # A file, not a pipe: grep -q exiting early would SIGPIPE the writer, and
  # pipefail would turn the match into a miss.
  plain "$so" "$se" |
    grep -v -E '^\++ |^[[:space:]]*[0-9]+ \| |^[[:space:]]*\| *~+[[:space:]]*$|^[[:space:]]*Line \|[[:space:]]*$' >"$run/match.txt"
  missing=""
  if [ -z "$expects" ]; then
    missing="(no # expect: line)"
  else
    while IFS= read -r e; do
      [ -n "$e" ] || continue
      if ! grep -F -q -- "$e" "$run/match.txt"; then missing="$missing${missing:+; }$e"; fi
    done <<<"$expects"
  fi

  result=pass
  if [ -n "${tcmd[*]:-}" ] && [ "$code" -eq 124 ]; then
    result="TIMEOUT (${timeout_s}s)"
  elif [ "$want_exit" != any ] && [ "$code" != "$want_exit" ]; then
    result="FAIL: exit $code"
  elif [ -n "$missing" ]; then
    result="FAIL: expect"
  fi
  [ "$result" = pass ] || failed=1

  expect_cell="ok"
  [ -n "$missing" ] && expect_cell="missing: $(printf '%s' "$missing" | html | sed 's/|/\&#124;/g')"
  rows="$rows| $i | <code>$(label "$f")</code> | $code | $expect_cell | ${secs}s | $result |"$'\n'
  printf '%s\t%s\t%s\t%s\t%s\n' "$i" "$result" "$code" "$secs" "$missing" >>"$results"

  gh_group "block $i: $result (exit $code, ${secs}s)"
  echo "--- block"
  cat "$f"
  echo "--- stdout"
  cat "$so"
  echo "--- stderr"
  cat "$se"
  [ -n "$missing" ] && echo "--- expect not found: $missing"
  gh_endgroup

  details="$details<details><summary>Block $i: $result (exit $code, ${secs}s)</summary>"$'\n\n'
  details="$details<pre>$(html <"$f")</pre>"$'\n\n'"stdout:"$'\n\n'"<pre>$(excerpt "$so" | html)</pre>"$'\n\n'
  details="$details""stderr:"$'\n\n'"<pre>$(excerpt "$se" | html)</pre>"$'\n\n'"</details>"$'\n\n'
done

verdict="all $count blocks passed"
[ "$failed" -ne 0 ] && verdict="FAILED"
total=$(awk -v m="$total_ms" 'BEGIN { printf "%.1f", m / 1000 }')
{
  echo "## Field rehearsal: $cell, $shell, $who: $verdict"
  echo
  echo "Cell: $(printf '%s' "$cell_facts" | html)"
  [ -n "${FIELD_SETUP:-}" ] && echo "" && echo "Cell setup (as root, before the blocks): <code>$(printf '%s' "$FIELD_SETUP" | html)</code>"
  echo
  echo "Each block ran in a new login shell (\`$shell\`), started in $home with a fresh environment. Total ${total}s."
  echo
  echo "| # | Block | Exit | Expect | Time | Result |"
  echo "|---|---|---|---|---|---|"
  printf '%s' "$rows"
  echo
  printf '%s' "$details"
} >"$summary"

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then cat "$summary" >>"$GITHUB_STEP_SUMMARY"; fi
echo
echo "run-blocks: $cell / $shell / $who: $verdict (${total}s)"
echo "run-blocks: cell: $cell_facts"
column -t -s "$(printf '\t')" "$results" 2>/dev/null || cat "$results"
echo "run-blocks: report in $summary"
exit "$failed"
