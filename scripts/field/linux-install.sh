#!/usr/bin/env bash
# Linux install matrix: install the PUBLISHED buildwithnexus (the GitHub
# release asset and the npm package) into clean distro containers and check
# that it runs where it is supported. Where it is not (glibc below the
# documented floor, musl), the bare release asset is expected to fail in the
# loader, before main, where nothing can explain anything; the npm launcher
# paths (non-TTY, bootstrap, TTY) must explain the failure in words.
#
#   bash scripts/field/linux-install.sh --version 0.14.9
#   bash scripts/field/linux-install.sh --version 0.14.3 --cells ubuntu:22.04,node:20-bullseye
#   bash scripts/field/linux-install.sh --version latest --json out.json --report-only
#   bash scripts/field/linux-install.sh --tarball buildwithnexus-0.14.9.tgz --release-yml .github/workflows/release.yml
#   bash scripts/field/linux-install.sh --self-test     # assertion logic only, no docker
#
# --tarball installs an unreleased package (npm pack) instead of the
# registry's; its launcher still downloads the published release asset for
# the version in its package.json. --release-yml names the release.yml whose
# glibc gate the docs are held to (default: the one at tag vX.Y.Z, else main).
# The release asset tested is the host's CPU (uname -m: x86_64 or aarch64).
# Every step runs in its own `docker exec` (or `docker run`), so nothing a
# previous step exported or sourced leaks into the next one, the way it would
# in one long shell session. Needs docker, curl, sha256sum and GNU sort.
# Exit: 0 all PASS/SKIP, 1 any FAIL (0 with --report-only), 2 usage/setup.
set -uo pipefail

REPO=Garretts-Apps/buildwithnexus
SITE_DOCS=https://buildwithnexus.dev/docs/install
# nvm's own documented install line, pinned to a tag.
NVM_VERSION=v0.40.8
STEP_TIMEOUT=${BWN_FIELD_STEP_TIMEOUT:-600}
# Where --tarball's package is mounted inside the cells.
PKG_IN_CELL=/tmp/bwn-field.tgz

BINARY_CELLS=(ubuntu:20.04 ubuntu:22.04 ubuntu:24.04 debian:11 debian:12
  amazonlinux:2 amazonlinux:2023 rockylinux:8 rockylinux:9 fedora:latest alpine:3)
NPM_CELLS=(node:20-bullseye node:22-bookworm node:22-alpine)
NVM_CELL=u2204-nvm
ALL_CELLS=("${BINARY_CELLS[@]}" "${NPM_CELLS[@]}" "$NVM_CELL")
# U12: after a working bootstrap, the binary loses its execute bit.
EACCES_CELL=node:22-bookworm

usage() {
  awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"
  echo "Cells: ${ALL_CELLS[*]}"
}

log() { printf '%s\n' "$*" >&2; }

# ---------------------------------------------------------------- results
R_CELL=() R_KIND=() R_LIBC=() R_RESULT=() R_DETAIL=()
add_row() { # cell kind libc result detail
  R_CELL+=("$1") R_KIND+=("$2") R_LIBC+=("$3") R_RESULT+=("$4")
  R_DETAIL+=("$(printf '%s' "$5" | tr '\n\t\r' '   ' | tr -d '\000-\037' | cut -c1-300)")
  log "  -> $1 [$2] $4: $5"
}

# Print captured output in a collapsed log group, so a FAIL can be read in
# the job log without re-running.
show_output() { # title text
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::group::$1" >&2; else log "---- $1"; fi
  printf '%s\n' "$2" >&2
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::endgroup::" >&2; fi
}

# ------------------------------------------------------------- assertions
# These operate on text only, so --self-test can run them without docker.

# The loader or exec failing, not anything bwn wrote.
LOADER_RE="version .GLIBC_[0-9.]+. not found|ENOENT|[Nn]o such file or directory|: not found\$|error while loading shared libraries|cannot execute"
# Raw text removed before looking for guidance: the loader's, and Node's
# bare "spawnSync <path> EACCES".
RAW_RE="$LOADER_RE|spawnSync [^ ]+ E[A-Z]+"
# The sentences the launcher prints for each cause (scripts/diagnose.js).
# Not single words: "requires" also matches "Cannot find module .../requires.js".
GUIDE_GLIBC='glibc is too old|needs glibc [0-9]+\.[0-9]+|requires glibc [0-9]+\.[0-9]+|glibc [0-9.]+ is not supported'
GUIDE_MUSL='uses musl|musl libc|no prebuilt musl|musl is not supported|not supported on musl'
GUIDE_EACCES='permission denied starting the binary|is not executable|mounted noexec'
# The 0.14.9 non-TTY hint: right on glibc at the floor, wrong where the
# download it offers cannot run.
BOOTSTRAP_HINT_RE='^[[:space:]]*bwn --bootstrap|BWN_ALLOW_BOOTSTRAP=1 bwn'

ver_ge() { [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -n1)" = "$2" ]; }

strip_ansi() { sed -e 's/\x1b\[[0-9;?]*[A-Za-z]//g' -e 's/\r//g'; }

# Short description of how a run failed, for the detail column.
symptom() { # output
  local missing raw last
  missing=$(printf '%s\n' "$1" | grep -oE "GLIBC_[0-9.]+' not found" | sed "s/' not found//" | sort -uV | paste -sd, -)
  if [ -n "$missing" ]; then echo "loader: $missing not found"; return; fi
  raw=$(printf '%s\n' "$1" | grep -oE 'spawnSync [^ ]+ E[A-Z]+' | head -n1 | sed -E 's/spawnSync [^ ]+ /spawnSync /')
  if [ -n "$raw" ]; then echo "$raw"; return; fi
  last=$(printf '%s\n' "$1" | grep -v '^[[:space:]]*$' | tail -n1 | sed 's/^[[:space:]]*//')
  echo "${last:-no output}"
}

# "glibc 2.35" | "musl" | "unknown" from `ldd --version` / getconf output.
parse_libc() { # probe-output
  if grep -qi 'musl' <<<"$1"; then echo musl; return; fi
  local v
  v=$(printf '%s\n' "$1" | grep -m1 -iE 'libc.*[0-9]+\.[0-9]+' | grep -oE '[0-9]+\.[0-9]+' | tail -n1)
  if [ -n "$v" ]; then echo "glibc $v"; else echo unknown; fi
}

join_semi() { local out='' p; for p in "$@"; do out="${out:+$out; }$p"; done; printf '%s' "$out"; }

# Guidance about <topic> in the output once the raw error text is removed,
# so guidance appended to the error line itself still counts.
has_guidance() { # topic output
  local re
  case $1 in
    glibc) re=$GUIDE_GLIBC ;;
    musl) re=$GUIDE_MUSL ;;
    eacces) re=$GUIDE_EACCES ;;
    *) return 1 ;;
  esac
  printf '%s\n' "$2" | sed -E "s/$RAW_RE//g" | grep -qiE "$re"
}

# Sets RESULT and DETAIL for a cell where the binary should run.
expect_runs() { # output rc version
  if [ "$2" -eq 0 ] && grep -qF "buildwithnexus $3" <<<"$1"; then
    RESULT=PASS DETAIL="buildwithnexus $3"
  else
    RESULT=FAIL DETAIL="exit $2, expected \"buildwithnexus $3\": $(symptom "$1")"
  fi
}

# Release asset run directly on an unsupported platform: the loader fails
# before main, so no program could explain it there. The loader failure is
# the expected result; anything else (a crash, a hang) is not.
expect_unsupported() { # output rc version why
  if [ "$2" -eq 0 ] && grep -qF "buildwithnexus $3" <<<"$1"; then
    RESULT=PASS DETAIL="runs although unsupported ($4)"
  elif [ "$2" -ne 0 ] && grep -qE "$LOADER_RE" <<<"$1"; then
    RESULT=PASS DETAIL="unsupported: $4; $(symptom "$1")"
  else
    RESULT=FAIL DETAIL="unsupported ($4) but no loader error: exit $2, $(symptom "$1")"
  fi
}

# Sets RESULT and DETAIL for an npm launcher path that cannot work here: it
# must fail, must not claim success first, and must say why in words.
expect_guided_failure() { # output rc version topic
  local out=$1 rc=$2 v=$3 topic=$4 problems=()
  if [ "$rc" -eq 0 ] && grep -qF "buildwithnexus $v" <<<"$out"; then
    RESULT=PASS DETAIL="runs although unsupported"
    return
  fi
  [ "$rc" -eq 0 ] && problems+=("exit 0 without running")
  grep -q 'is ready' <<<"$out" && problems+=('"is ready" printed before the failure')
  has_guidance "$topic" "$out" || problems+=("no $topic guidance beyond the raw error")
  if [ ${#problems[@]} -eq 0 ]; then
    RESULT=PASS DETAIL="fails with $topic guidance (exit $rc)"
  else
    RESULT=FAIL DETAIL=$(join_semi "${problems[@]}" "$(symptom "$out")")
  fi
}

# Sets RESULT and DETAIL for the non-TTY launcher on a supported platform:
# it must not download on its own and must print the --bootstrap hint.
expect_bootstrap_hint() { # output rc
  local problems=()
  [ "$2" -eq 1 ] || problems+=("exit $2, expected 1")
  grep -q -- '--bootstrap' <<<"$1" && grep -q 'BWN_ALLOW_BOOTSTRAP' <<<"$1" ||
    problems+=("no --bootstrap / BWN_ALLOW_BOOTSTRAP hint")
  grep -qi 'downloading' <<<"$1" && problems+=("downloaded without consent")
  if [ ${#problems[@]} -eq 0 ]; then
    RESULT=PASS DETAIL="exit 1 with the --bootstrap hint"
  else
    RESULT=FAIL DETAIL=$(join_semi "${problems[@]}" "$(symptom "$1")")
  fi
}

# The non-TTY launcher on an unsupported platform: the --bootstrap hint
# would send the user to download a binary that cannot run, so it must
# explain the platform instead.
expect_platform_guidance() { # output rc topic
  local problems=()
  [ "$2" -ne 0 ] || problems+=("exit 0")
  grep -qi 'downloading' <<<"$1" && problems+=("downloaded a binary that cannot run here")
  grep -qE -- "$BOOTSTRAP_HINT_RE" <<<"$1" && problems+=("suggests --bootstrap, whose download cannot run here")
  has_guidance "$3" "$1" || problems+=("no $3 guidance")
  if [ ${#problems[@]} -eq 0 ]; then
    RESULT=PASS DETAIL="exit $2 with $3 guidance, no --bootstrap hint"
  else
    RESULT=FAIL DETAIL=$(join_semi "${problems[@]}" "$(symptom "$1")")
  fi
}

# Floors stated in a doc, e.g. "glibc 2.35 or later", "glibc 2.35+".
doc_floors() { # text
  printf '%s\n' "$1" | grep -ioE 'glibc[^0-9<>|]{0,12}[0-9]+\.[0-9]+' | grep -oE '[0-9]+\.[0-9]+$' | sort -uV
}

# The floor release.yml's "Check the glibc floor" step holds every Linux
# build to: a literal (0.14.9: `"$need" 2.35`), or GLIBC_FLOOR read from
# scripts/diagnose.js, inline or through scripts/check-glibc-floor.sh.
# Empty when the step is not there.
GATE_FROM_DIAG_RE='GLIBC_FLOOR|check-glibc-floor'
gate_floor() { # release.yml-text diagnose.js-text
  local step
  step=$(printf '%s\n' "$1" | sed -n '/name: Check the glibc floor/,/^[[:space:]]*- name:/p' | sed '1!{/- name:/d;}')
  [ -n "$step" ] || return 0
  if grep -qE "$GATE_FROM_DIAG_RE" <<<"$step"; then
    printf '%s\n' "$2" | grep -oE "GLIBC_FLOOR *= *['\"][0-9]+\.[0-9]+['\"]" | grep -oE '[0-9]+\.[0-9]+' | head -n1
  else
    # shellcheck disable=SC2016  # a literal $need in the workflow text
    printf '%s\n' "$step" | grep -oE '"\$need" [0-9]+\.[0-9]+' | grep -oE '[0-9]+\.[0-9]+$' | head -n1
  fi
}

# Sets RESULT and DETAIL for the docs' glibc floor. FAIL when the binary
# needs more than a doc states (users at that floor get a loader error, as
# in U11), or when a doc differs from the release.yml gate, the number each
# release is held to. Docs stating more than the binary needs are fine: the
# gate leaves room for a build that picks up a newer symbol.
docs_floor_verdict() { # bin-floor gate-floor "stated floors" notes
  local bin=$1 gate=$2 notes=$4 f low='' problems=()
  if [ -z "$bin" ]; then RESULT=SKIP DETAIL="binary floor unknown (no asset); docs: $notes"; return; fi
  for f in $3; do if [ -z "$low" ] || ! ver_ge "$f" "$low"; then low=$f; fi; done
  if [ -z "$low" ]; then
    problems+=("binary needs GLIBC_$bin but no doc states a glibc version")
  elif ! ver_ge "$low" "$bin"; then
    problems+=("binary needs GLIBC_$bin, more than the $low the docs state")
  fi
  if [ -n "$gate" ]; then
    for f in $3; do
      [ "$f" = "$gate" ] || { problems+=("docs differ from the release.yml gate $gate"); break; }
    done
  fi
  if [ ${#problems[@]} -eq 0 ]; then
    RESULT=PASS DETAIL="binary needs $bin; release.yml gate ${gate:-not found}; docs: $notes"
  else
    RESULT=FAIL DETAIL="$(join_semi "${problems[@]}") (docs: $notes)"
  fi
}

# What a cell should do, given its libc. Sets EXPECT (runs|unsupported),
# TOPIC (the guidance the launcher owes: glibc|musl), WHY and NOTE.
classify() { # libc
  EXPECT=runs TOPIC='' WHY='' NOTE=''
  case $1 in
    musl) EXPECT=unsupported TOPIC=musl WHY="musl libc" ;;
    glibc\ *)
      local v=${1#glibc }
      if [ -z "$DOC_FLOOR" ] || ver_ge "$v" "$DOC_FLOOR"; then
        # Documented as supported, so it has to run (U11 when it does not).
        if [ -n "$BIN_FLOOR" ] && ! ver_ge "$v" "$BIN_FLOOR"; then
          NOTE="documented as supported (glibc ${DOC_FLOOR:-any}+) but the binary needs $BIN_FLOOR; "
        fi
      elif [ -n "$BIN_FLOOR" ] && ver_ge "$v" "$BIN_FLOOR"; then
        NOTE="below the documented floor $DOC_FLOOR; "
      else
        EXPECT=unsupported TOPIC=glibc WHY="glibc $v is below the documented $DOC_FLOOR"
      fi
      ;;
  esac
}

# Judge one run of the release asset (path=asset) or the npm launcher.
judge() { # output rc path
  if [ "$EXPECT" = runs ]; then
    expect_runs "$1" "$2" "$VERSION"
  elif [ "$3" = asset ]; then
    expect_unsupported "$1" "$2" "$VERSION" "$WHY"
  else
    expect_guided_failure "$1" "$2" "$VERSION" "$TOPIC"
    DETAIL="$WHY; $DETAIL"
  fi
  DETAIL="$NOTE$DETAIL"
}

self_test() {
  local fails=0 v=0.14.9
  check() { # name got-result want-result
    if [ "$2" = "$3" ]; then log "ok   $1"; else log "FAIL $1: got $2, want $3 (${DETAIL:-})"; fails=$((fails + 1)); fi
  }
  VERSION=$v
  # Real outputs from 0.14.9 (scratch runs on 2026-09-29) and from the
  # launcher fix (field/launcher, 2026-09-30).
  local ready bullseye alpine raw guided hint eacces fix_glibc fix_musl fix_eacces
  ready=$'buildwithnexus: installed prebuilt binary (sha256 verified).\n\n  \e[38;5;141mbuildwithnexus\e[0m is ready.\n  Run  buildwithnexus  - the first launch walks you through choosing a model:\n'
  bullseye="$ready/usr/local/lib/node_modules/buildwithnexus/bin/buildwithnexus: /lib/x86_64-linux-gnu/libc.so.6: version \`GLIBC_2.32' not found (required by /usr/local/lib/node_modules/buildwithnexus/bin/buildwithnexus)
/usr/local/lib/node_modules/buildwithnexus/bin/buildwithnexus: /lib/x86_64-linux-gnu/libc.so.6: version \`GLIBC_2.34' not found (required by /usr/local/lib/node_modules/buildwithnexus/bin/buildwithnexus)"
  alpine="${ready}buildwithnexus: spawnSync /usr/local/lib/node_modules/buildwithnexus/bin/buildwithnexus ENOENT"
  raw="/bwn: /lib64/libc.so.6: version \`GLIBC_2.29' not found (required by /bwn)"
  guided="$raw
buildwithnexus needs glibc 2.34 or later; this system has 2.28. Upgrade the OS or build from source."
  hint=$'buildwithnexus: native binary not found.\n  Download the checksum-verified release binary once (no terminal here, so the\n  launcher will not download it on its own):\n    bwn --bootstrap        (or BWN_ALLOW_BOOTSTRAP=1 bwn)\n'
  eacces="buildwithnexus: spawnSync /usr/local/lib/node_modules/buildwithnexus/bin/buildwithnexus EACCES"
  fix_glibc=$'buildwithnexus: installed prebuilt binary (sha256 verified).\n\nbuildwithnexus: this system\'s glibc is too old for the prebuilt binary.\n  It needs glibc 2.34 or later; this system has 2.31.\n  Loader error: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.34\' not found\n  Docs: https://buildwithnexus.dev/docs/install'
  fix_musl=$'buildwithnexus: this system uses musl libc (Alpine or similar), and the prebuilt\n  Linux binary needs glibc 2.34+. There is no prebuilt musl binary yet. Either:\n  Docs: https://buildwithnexus.dev/docs/install'
  fix_eacces=$'buildwithnexus: permission denied starting the binary (EACCES).\n  The file is not executable, or it is on a filesystem mounted noexec.\n  Fix:   chmod +x \'/usr/local/lib/node_modules/buildwithnexus/bin/buildwithnexus\''

  # npm launcher paths on unsupported platforms: guidance or FAIL.
  expect_guided_failure "$(strip_ansi <<<"$bullseye")" 1 $v glibc; check "bullseye: ready then GLIBC" "$RESULT" FAIL
  case $DETAIL in *'is ready'*GLIBC_2.32,GLIBC_2.34*) ;; *) check "bullseye detail names both" x y ;; esac
  expect_guided_failure "$alpine" 1 $v musl; check "alpine: ENOENT" "$RESULT" FAIL
  case $DETAIL in *ENOENT*) ;; *) check "alpine detail names ENOENT" x y ;; esac
  expect_guided_failure "$raw" 1 $v glibc; check "raw loader line alone" "$RESULT" FAIL
  expect_guided_failure "$guided" 1 $v glibc; check "loader line plus a sentence" "$RESULT" PASS
  expect_guided_failure "buildwithnexus: spawnSync /x ENOENT: this system uses musl; prebuilt binaries need glibc 2.34+" 1 $v musl
  check "ENOENT and guidance on one line" "$RESULT" PASS
  expect_guided_failure "/bwn: /lib64/libc.so.6: version \`GLIBC_2.34' not found (required by /bwn); buildwithnexus requires glibc 2.34" 1 $v glibc
  check "loader error and guidance on one line" "$RESULT" PASS
  expect_guided_failure "$fix_glibc" 1 $v glibc; check "launcher fix: glibc guidance" "$RESULT" PASS
  expect_guided_failure "$fix_musl" 1 $v musl; check "launcher fix: musl guidance" "$RESULT" PASS
  expect_guided_failure "$fix_glibc" 1 $v musl; check "glibc guidance on a musl cell" "$RESULT" FAIL
  expect_guided_failure "Error: Cannot find module '/usr/local/lib/node_modules/buildwithnexus/scripts/requires.js'" 1 $v glibc
  check "unrelated crash mentioning requires" "$RESULT" FAIL
  expect_guided_failure "Error: buildwithnexus requires a terminal" 1 $v musl; check "bare 'requires'" "$RESULT" FAIL
  expect_guided_failure "buildwithnexus $v" 0 $v glibc; check "launcher runs below floor" "$RESULT" PASS
  # U12: bare spawnSync EACCES vs the explanation.
  expect_guided_failure "$eacces" 1 $v eacces; check "bare spawnSync EACCES" "$RESULT" FAIL
  case $DETAIL in *'spawnSync EACCES'*) ;; *) check "EACCES detail" x y ;; esac
  expect_guided_failure "$fix_eacces" 1 $v eacces; check "EACCES guidance" "$RESULT" PASS
  # Release asset on unsupported platforms: the loader failure is expected.
  expect_unsupported "$raw" 1 $v "glibc 2.28"; check "asset: loader error" "$RESULT" PASS
  expect_unsupported "sh: /opt/bwn/buildwithnexus: not found" 127 $v musl; check "asset: musl exec not found" "$RESULT" PASS
  expect_unsupported "Segmentation fault (core dumped)" 139 $v "glibc 2.28"; check "asset: crash is not a loader error" "$RESULT" FAIL
  expect_unsupported "buildwithnexus $v" 0 $v "glibc 2.31"; check "asset: runs anyway" "$RESULT" PASS
  expect_runs "buildwithnexus $v" 0 $v; check "runs" "$RESULT" PASS
  expect_runs "buildwithnexus 0.14.8" 0 $v; check "wrong version" "$RESULT" FAIL
  expect_runs "$raw" 1 $v; check "supported cell hits loader" "$RESULT" FAIL
  # Non-TTY launcher.
  expect_bootstrap_hint "$hint" 1; check "non-TTY hint" "$RESULT" PASS
  expect_bootstrap_hint "buildwithnexus: downloading prebuilt binary" 0; check "non-TTY download" "$RESULT" FAIL
  expect_platform_guidance "$hint" 1 musl; check "musl non-TTY: --bootstrap hint" "$RESULT" FAIL
  expect_platform_guidance "$fix_musl" 1 musl; check "musl non-TTY: guidance" "$RESULT" PASS
  expect_platform_guidance "$fix_musl"$'\n'"$hint" 1 musl; check "musl non-TTY: guidance and hint" "$RESULT" FAIL
  expect_platform_guidance "$fix_glibc" 1 glibc; check "old glibc non-TTY: guidance" "$RESULT" PASS
  # Which cells must run.
  DOC_FLOOR=2.35 BIN_FLOOR=2.34
  classify musl; check "classify musl" "$EXPECT/$TOPIC" unsupported/musl
  classify "glibc 2.31"; check "classify below both" "$EXPECT/$TOPIC" unsupported/glibc
  classify "glibc 2.34"; check "classify between" "$EXPECT" runs
  classify "glibc 2.35"; check "classify at doc floor" "$EXPECT/$NOTE" runs/
  DOC_FLOOR=2.35 BIN_FLOOR=2.39
  classify "glibc 2.35"; check "classify U11" "$EXPECT/${NOTE%% (*}" "runs/documented as supported"
  judge "$raw" 1 asset; check "U11 asset cell fails" "$RESULT" FAIL
  DOC_FLOOR=2.34 BIN_FLOOR=2.34
  classify "glibc 2.31"; judge "$raw" 1 asset; check "judge asset below floor" "$RESULT" PASS
  case $DETAIL in 'unsupported: glibc 2.31 is below the documented 2.34; loader: GLIBC_2.29 not found') ;; *) check "unsupported detail" x y ;; esac
  judge "$fix_glibc" 1 launcher; check "judge launcher below floor" "$RESULT" PASS
  judge "$(strip_ansi <<<"$bullseye")" 1 launcher; check "judge launcher, 0.14.9" "$RESULT" FAIL
  # Docs floor.
  docs_floor_verdict 2.34 2.35 "2.35 2.35" x; check "docs: 0.14.9 (docs = gate > binary)" "$RESULT" PASS
  docs_floor_verdict 2.34 2.34 "2.34 2.34" x; check "docs: all 2.34" "$RESULT" PASS
  docs_floor_verdict 2.34 2.35 "2.34" x; check "docs: differ from gate" "$RESULT" FAIL
  docs_floor_verdict 2.34 2.34 "2.34 2.35" x; check "docs: one stale doc" "$RESULT" FAIL
  docs_floor_verdict 2.39 "" "2.35" x; check "docs: U11 (binary needs more)" "$RESULT" FAIL
  docs_floor_verdict 2.35 2.35 "" x; check "docs: no version stated" "$RESULT" FAIL
  docs_floor_verdict "" 2.35 "2.35" x; check "docs: no asset" "$RESULT" SKIP
  local rel149 relfix diag
  rel149=$'      - name: Check the glibc floor\n        run: |\n          need=$(readelf -V x | tail -1)\n          if [ "$(printf \'%s\\n\' "$need" 2.35 | sort -V | tail -1)" != "2.35" ]; then\n            exit 1\n          fi\n\n      - name: Hand the asset to the assets job\n        run: echo 2.99'
  relfix=$'      - name: Check the glibc floor\n        run: |\n          floor=$(node -p "require(\'./scripts/diagnose.js\').GLIBC_FLOOR")\n          if [ "$(printf \'%s\\n\' "$need" "$floor" | sort -V | tail -1)" != "$floor" ]; then exit 1; fi\n      - name: Next\n        run: echo "$need" 2.99'
  diag="const GLIBC_FLOOR = '2.34';"
  check "gate: literal" "$(gate_floor "$rel149" "")" 2.35
  check "gate: from diagnose.js" "$(gate_floor "$relfix" "$diag")" 2.34
  check "gate: check-glibc-floor.sh" "$(gate_floor $'      - name: Check the glibc floor\n        run: scripts/check-glibc-floor.sh "buildwithnexus-$TARGET"\n\n      - name: Next\n        run: echo "$need" 2.99' "$diag")" 2.34
  check "gate: no step" "$(gate_floor $'jobs:\n  - name: Build\n    run: echo "$need" 2.35' "$diag")" ""
  check "libc ubuntu" "$(parse_libc 'ldd (Ubuntu GLIBC 2.35-0ubuntu3.14) 2.35')" "glibc 2.35"
  check "libc amzn" "$(parse_libc 'ldd (GNU libc) 2.26')" "glibc 2.26"
  check "libc musl" "$(parse_libc $'musl libc (x86_64)\nVersion 1.2.5')" musl
  check "libc getconf" "$(parse_libc 'glibc 2.34')" "glibc 2.34"
  check "floor README" "$(doc_floors '- **Prebuilt binaries:** Linux x64 and arm64 (glibc 2.35 or later: Ubuntu')" 2.35
  check "floor site" "$(doc_floors 'x86_64, aarch64 (prebuilt; glibc 2.35+, e.g. Ubuntu 22.04+')" 2.35
  check "floor none" "$(doc_floors 'Linux x64 and arm64 (glibc), macOS')" ""
  check "floor prose" "$(doc_floors 'the first run says why (glibc too old, musl, or blocked')" ""
  check "ver_ge equal" "$(ver_ge 2.35 2.35 && echo y)" y
  check "ver_ge lower" "$(ver_ge 2.31 2.34 && echo y || echo n)" n
  check "ver_ge minor" "$(ver_ge 2.4 2.34 && echo y || echo n)" n
  if [ "$fails" -eq 0 ]; then log "self-test: all passed"; else log "self-test: $fails failed"; fi
  [ "$fails" -eq 0 ]
}

# ------------------------------------------------------------------ args
VERSION='' CELLS='' JSON_OUT='' REPORT_ONLY=0 TARBALL='' RELEASE_YML=''
while [ $# -gt 0 ]; do
  case $1 in
    --version) VERSION=${2:-}; shift 2 ;;
    --cells) CELLS=${2:-}; shift 2 ;;
    --json) JSON_OUT=${2:-}; shift 2 ;;
    --tarball) TARBALL=${2:-}; shift 2 ;;
    --release-yml) RELEASE_YML=${2:-}; shift 2 ;;
    --report-only) REPORT_ONLY=1; shift ;;
    --self-test) self_test; exit $? ;;
    -h|--help) usage; exit 0 ;;
    *) log "unknown argument: $1"; usage >&2; exit 2 ;;
  esac
done
[ -n "$VERSION" ] || [ -n "$TARBALL" ] || { usage >&2; exit 2; }
for tool in docker curl sha256sum sort timeout tar; do
  command -v "$tool" >/dev/null || { log "missing required tool: $tool"; exit 2; }
done
if [ -n "$RELEASE_YML" ] && [ ! -f "$RELEASE_YML" ]; then log "no such file: $RELEASE_YML"; exit 2; fi

case $(uname -m) in
  x86_64|amd64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) log "no Linux release asset for $(uname -m)"; exit 2 ;;
esac
ASSET=buildwithnexus-$ARCH-unknown-linux-gnu

RUN_ID="bwn-field-$$"
WORK=$(mktemp -d)
PKG_DIR="$WORK/pkg"
mkdir -p "$PKG_DIR"
# shellcheck disable=SC2317,SC2329  # invoked by the EXIT trap
cleanup() {
  docker ps -aq --filter "label=bwn-field=$RUN_ID" 2>/dev/null | xargs -r docker rm -f >/dev/null 2>&1
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

LATEST=$(curl -fsSL --retry 3 https://registry.npmjs.org/buildwithnexus/latest 2>/dev/null |
  grep -oE '"version" *: *"[^"]+"' | head -n1 | sed -E 's/.*"([^"]+)"$/\1/')
PKG_MOUNT=()
if [ -n "$TARBALL" ]; then
  [ -f "$TARBALL" ] || { log "no such tarball: $TARBALL"; exit 2; }
  TARBALL=$(cd "$(dirname "$TARBALL")" && pwd)/$(basename "$TARBALL")
  tar -xzf "$TARBALL" -C "$PKG_DIR" 2>/dev/null || { log "not an npm tarball: $TARBALL"; exit 2; }
  tar_version=$(sed -nE 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' "$PKG_DIR/package/package.json" 2>/dev/null | head -n1)
  if [ -n "$VERSION" ] && [ "$VERSION" != "$tar_version" ]; then
    log "--version $VERSION but the tarball is $tar_version"; exit 2
  fi
  VERSION=$tar_version
  INSTALL_SPEC=$PKG_IN_CELL
  PKG_MOUNT=(-v "$TARBALL:$PKG_IN_CELL:ro")
  SOURCE="tarball $(basename "$TARBALL")"
else
  if [ "$VERSION" = latest ]; then VERSION=$LATEST; log "latest on npm: ${VERSION:-?}"; fi
  INSTALL_SPEC="buildwithnexus@$VERSION"
  SOURCE=npm
fi
# The version is interpolated into container shell commands, so it must be
# a plain semver and nothing else.
if ! [[ $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  log "bad version: '$VERSION'"; exit 2
fi

SELECTED=()
if [ -z "$CELLS" ]; then
  SELECTED=("${ALL_CELLS[@]}")
else
  IFS=, read -r -a SELECTED <<<"$CELLS"
  for c in "${SELECTED[@]}"; do
    [[ " ${ALL_CELLS[*]} " == *" $c "* ]] || { log "unknown cell: $c (known: ${ALL_CELLS[*]})"; exit 2; }
  done
fi
selected() { [[ " ${SELECTED[*]} " == *" $1 "* ]]; }

# ---------------------------------------------------------------- docker
# Local runs (not CI) may have no daemon yet; start one when we can.
if ! docker info >/dev/null 2>&1; then
  if [ -z "${CI:-}" ] && [ "$(id -u)" = 0 ] && command -v dockerd >/dev/null; then
    dlog=${BWN_FIELD_DOCKERD_LOG:-${TMPDIR:-/tmp}/dockerd-field.log}
    log "starting dockerd (log: $dlog)"
    nohup dockerd >"$dlog" 2>&1 &
    for _ in $(seq 1 60); do docker info >/dev/null 2>&1 && break; sleep 1; done
  fi
  docker info >/dev/null 2>&1 || { log "docker daemon is not reachable"; exit 2; }
fi

# Networked cells get the host's proxy and CA only when the host has them
# (an egress-proxied dev container). GitHub-hosted runners set neither, so
# there the cells use plain bridge networking.
NET_ARGS=()
proxy_on_loopback=0
for var in HTTPS_PROXY https_proxy HTTP_PROXY http_proxy NO_PROXY no_proxy; do
  if [ -n "${!var:-}" ]; then
    NET_ARGS+=(-e "$var")
    case ${!var} in *://127.*|*://localhost*|*://\[::1\]*) proxy_on_loopback=1 ;; esac
  fi
done
# A proxy on the host's loopback is only reachable from the host network.
[ "$proxy_on_loopback" = 1 ] && NET_ARGS+=(--network host)
for var in NODE_EXTRA_CA_CERTS SSL_CERT_FILE CURL_CA_BUNDLE; do
  ca=${!var:-}
  if [ -n "$ca" ] && [ -f "$ca" ]; then
    NET_ARGS+=(-v "$ca:/etc/bwn-field-ca.crt:ro"
      -e NODE_EXTRA_CA_CERTS=/etc/bwn-field-ca.crt -e npm_config_cafile=/etc/bwn-field-ca.crt
      -e SSL_CERT_FILE=/etc/bwn-field-ca.crt -e CURL_CA_BUNDLE=/etc/bwn-field-ca.crt)
    break
  fi
done

# Pull with retries. A pull that never succeeds makes the cell SKIP; an
# older local copy is used if one exists (Docker Hub rate limits are common).
PULL_NOTE=''
pull() { # image -> 0 usable
  PULL_NOTE=''
  local i
  for i in 1 2 3; do
    docker pull -q "$1" >/dev/null 2>&1 && return 0
    log "  pull $1 failed (attempt $i)"
    sleep $((i * 5))
  done
  if docker image inspect "$1" >/dev/null 2>&1; then PULL_NOTE=" (pull failed; used cached image)"; return 0; fi
  return 1
}

# Start an idle container for a cell. Extra docker-run args first.
start_cell() { # image [docker run args...]
  local image=$1; shift
  docker run -d --label "bwn-field=$RUN_ID" --entrypoint "" "$@" "$image" tail -f /dev/null
}

# One step = one fresh `docker exec`. Sets OUT (ANSI and CR stripped) and RC.
OUT='' RC=0
xrun() { # cid [docker exec args...] -- argv...
  local cid=$1 opts=()
  shift
  while [ $# -gt 0 ] && [ "$1" != -- ]; do opts+=("$1"); shift; done
  shift
  OUT=$(timeout "$STEP_TIMEOUT" docker exec ${opts[@]+"${opts[@]}"} "$cid" "$@" 2>&1)
  RC=$?
  OUT=$(printf '%s' "$OUT" | strip_ansi)
}

probe_libc() { # cid -> prints parsed libc
  xrun "$1" -- sh -lc 'ldd --version 2>&1 | head -n2; getconf GNU_LIBC_VERSION 2>/dev/null'
  parse_libc "$OUT"
}

# ---------------------------------------------------- published artifacts
log "== buildwithnexus $VERSION ($SOURCE, $ARCH): release asset and docs"
base="https://github.com/$REPO/releases/download/v$VERSION"
ASSET_PATH="$WORK/$ASSET"
ASSET_SHA=''
BIN_FLOOR=''
if curl -fsSL --retry 3 -o "$ASSET_PATH" "$base/$ASSET" &&
  curl -fsSL --retry 3 -o "$ASSET_PATH.sha256" "$base/$ASSET.sha256"; then
  want=$(awk '{print $1; exit}' "$ASSET_PATH.sha256")
  got=$(sha256sum "$ASSET_PATH" | awk '{print $1}')
  chmod 755 "$ASSET_PATH"
  # The highest GLIBC_x.y version the binary references is its real floor.
  if command -v readelf >/dev/null; then
    BIN_FLOOR=$(readelf -V "$ASSET_PATH" 2>/dev/null | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' | sed 's/GLIBC_//' | sort -uV | tail -n1)
  fi
  [ -n "$BIN_FLOOR" ] ||
    BIN_FLOOR=$(grep -aoE 'GLIBC_[0-9]+(\.[0-9]+)+' "$ASSET_PATH" | sed 's/GLIBC_//' | sort -uV | tail -n1)
  if [ "$want" = "$got" ]; then
    ASSET_SHA=$got
  else
    add_row release asset - FAIL "sha256 $got does not match $ASSET.sha256 ($want)"
  fi
else
  add_row release asset - FAIL "could not download $ASSET or its .sha256 for v$VERSION"
fi

# Docs as the user sees them: the README and SECURITY.md shipped in the
# package under test, and the live install page the launcher links to.
if [ -z "$TARBALL" ] &&
  curl -fsSL --retry 3 -o "$WORK/pkg.tgz" "https://registry.npmjs.org/buildwithnexus/-/buildwithnexus-$VERSION.tgz"; then
  tar -xzf "$WORK/pkg.tgz" -C "$PKG_DIR" 2>/dev/null
fi
if [ -n "$ASSET_SHA" ]; then
  pinned=''
  [ -f "$PKG_DIR/package/checksums.json" ] &&
    pinned=$(grep -oE "\"$ASSET\" *: *\"[0-9a-f]{64}\"" "$PKG_DIR/package/checksums.json" | grep -oE '[0-9a-f]{64}')
  if [ -z "$pinned" ]; then
    add_row release asset - PASS "$ASSET: sha256 matches .sha256 (no checksums.json entry in the package); needs GLIBC_${BIN_FLOOR:-?}"
  elif [ "$pinned" = "$ASSET_SHA" ]; then
    add_row release asset - PASS "$ASSET: sha256 matches .sha256 and the package's checksums.json; needs GLIBC_${BIN_FLOOR:-?}"
  else
    add_row release asset - FAIL "$ASSET: sha256 differs from the package's checksums.json ($pinned)"
  fi
fi

# The live site describes npm's latest, so it is held to that version only:
# not to an older one (a backtest) or an unreleased tarball.
doc_sources=(README.md SECURITY.md)
site_note=''
if [ -z "$TARBALL" ] && [ "$VERSION" = "$LATEST" ]; then
  doc_sources+=("$SITE_DOCS")
else
  site_note="site not checked (it describes ${LATEST:-npm latest}${TARBALL:+, not this tarball})"
fi
DOC_FLOOR=''   # lowest floor any doc states; empty if none states one
floor_notes=() stated=()
for src in "${doc_sources[@]}"; do
  if [ "$src" = "$SITE_DOCS" ]; then
    text=$(curl -fsSL --retry 2 "$src" 2>/dev/null | sed 's/<[^>]*>/ /g')
    name=${src#https://}
  else
    text=$(cat "$PKG_DIR/package/$src" 2>/dev/null)
    name=$src
  fi
  if [ -z "$text" ]; then floor_notes+=("$name: unreadable"); continue; fi
  floors=$(doc_floors "$text" | paste -sd/ -)
  if ! grep -qi glibc <<<"$text"; then continue; fi
  floor_notes+=("$name: ${floors:-glibc, no version}")
  for f in $(doc_floors "$text"); do
    stated+=("$f")
    if [ -z "$DOC_FLOOR" ] || ! ver_ge "$f" "$DOC_FLOOR"; then DOC_FLOOR=$f; fi
  done
done
[ -n "$site_note" ] && floor_notes+=("$site_note")

# release.yml's glibc gate for this version: the tag's, else main's (a
# version not tagged yet), or the file given with --release-yml.
rel_text='' GATE_SRC='' gate_ref=''
if [ -n "$RELEASE_YML" ]; then
  rel_text=$(cat "$RELEASE_YML") GATE_SRC=$RELEASE_YML
else
  for ref in "v$VERSION" main; do
    if rel_text=$(curl -fsSL --retry 2 "https://raw.githubusercontent.com/$REPO/$ref/.github/workflows/release.yml" 2>/dev/null); then
      GATE_SRC="release.yml@$ref" gate_ref=$ref
      break
    fi
  done
fi
# A gate that reads GLIBC_FLOOR gets it from the package's diagnose.js
# (shipped from the same commit), else from the same ref.
diag_text=$(cat "$PKG_DIR/package/scripts/diagnose.js" 2>/dev/null)
if [ -z "$diag_text" ] && [ -n "$gate_ref" ] && grep -qE "$GATE_FROM_DIAG_RE" <<<"$rel_text"; then
  diag_text=$(curl -fsSL --retry 2 "https://raw.githubusercontent.com/$REPO/$gate_ref/scripts/diagnose.js" 2>/dev/null)
fi
GATE_FLOOR=$(gate_floor "$rel_text" "$diag_text")
[ -n "$GATE_SRC" ] || GATE_SRC="release.yml unreadable"

notes=$(join_semi ${floor_notes[@]+"${floor_notes[@]}"})
docs_floor_verdict "$BIN_FLOOR" "$GATE_FLOOR" "${stated[*]+${stated[*]}}" "$notes"
add_row docs glibc-floor - "$RESULT" "$DETAIL (gate from $GATE_SRC)"

# ------------------------------------------------------------ binary cells
for img in "${BINARY_CELLS[@]}"; do
  selected "$img" || continue
  log "== $img (release asset)"
  if [ -z "$ASSET_SHA" ]; then add_row "$img" release-asset - SKIP "no verified asset"; continue; fi
  if ! pull "$img"; then add_row "$img" release-asset - SKIP "docker pull failed after 3 attempts"; continue; fi
  # --version needs no network, so these cells get none.
  cid=$(start_cell "$img" --network none -v "$ASSET_PATH:/opt/bwn/buildwithnexus:ro") ||
    { add_row "$img" release-asset - SKIP "container did not start"; continue; }
  libc=$(probe_libc "$cid")
  xrun "$cid" -- sh -lc 'sha256sum /opt/bwn/buildwithnexus 2>/dev/null'
  in_cell=${OUT%% *}
  if [ -n "$in_cell" ] && [ "$in_cell" != "$ASSET_SHA" ]; then
    add_row "$img" release-asset "$libc" FAIL "asset changed inside the container ($in_cell)"
  else
    xrun "$cid" -- sh -lc '/opt/bwn/buildwithnexus --version'
    classify "$libc"
    judge "$OUT" "$RC" asset
    [ "$RESULT" = FAIL ] && show_output "$img release-asset output (exit $RC)" "$OUT"
    add_row "$img" release-asset "$libc" "$RESULT" "$DETAIL$PULL_NOTE"
  fi
  docker rm -f "$cid" >/dev/null 2>&1
done

# --------------------------------------------------------------- npm cells
npm_install() { # cid [exec args] -> 0 ok; retries once for registry flakes
  local cid=$1; shift
  xrun "$cid" "$@" -- sh -lc "npm i -g $INSTALL_SPEC"
  [ "$RC" -eq 0 ] && return 0
  sleep 10
  xrun "$cid" "$@" -- sh -lc "npm i -g $INSTALL_SPEC"
  [ "$RC" -eq 0 ]
}

# U12: a binary that cannot be executed (lost mode bits, a noexec mount)
# must be explained, not reported as "spawnSync <path> EACCES".
eacces_step() { # cid libc
  local cid=$1 libc=$2 bin
  xrun "$cid" -- sh -lc "cd \"\$(npm root -g)/buildwithnexus\" && node -p \"require('./scripts/resolve-binary.js').existing() || ''\""
  bin=$(printf '%s\n' "$OUT" | tail -n1)
  if [ "$RC" -ne 0 ] || [ "${bin#/}" = "$bin" ]; then
    add_row "$EACCES_CELL" npm:eacces "$libc" SKIP "could not locate the installed binary: $(symptom "$OUT")"
    return
  fi
  xrun "$cid" -- chmod a-x "$bin"
  if [ "$RC" -ne 0 ]; then add_row "$EACCES_CELL" npm:eacces "$libc" SKIP "chmod a-x failed: $OUT"; return; fi
  xrun "$cid" -- sh -lc 'bwn --version'
  expect_guided_failure "$OUT" "$RC" "$VERSION" eacces
  [ "$RESULT" = FAIL ] && show_output "$EACCES_CELL chmod a-x output (exit $RC)" "$OUT"
  add_row "$EACCES_CELL" npm:eacces "$libc" "$RESULT" "binary chmod a-x; $DETAIL"
}

for img in "${NPM_CELLS[@]}"; do
  selected "$img" || continue
  log "== $img (npm, $INSTALL_SPEC)"
  if ! pull "$img"; then add_row "$img" npm - SKIP "docker pull failed after 3 attempts"; continue; fi

  # Container 1: non-TTY without consent, then with BWN_ALLOW_BOOTSTRAP=1.
  cid=$(start_cell "$img" ${NET_ARGS[@]+"${NET_ARGS[@]}"} ${PKG_MOUNT[@]+"${PKG_MOUNT[@]}"}) ||
    { add_row "$img" npm - SKIP "container did not start"; continue; }
  libc=$(probe_libc "$cid")
  classify "$libc"
  if ! npm_install "$cid"; then
    show_output "$img npm install (exit $RC)" "$OUT"
    add_row "$img" npm "$libc" FAIL "npm i -g $INSTALL_SPEC failed: $(symptom "$OUT")"
    docker rm -f "$cid" >/dev/null 2>&1
    continue
  fi
  xrun "$cid" -- sh -lc 'bwn --version'
  if [ "$EXPECT" = runs ]; then
    expect_bootstrap_hint "$OUT" "$RC"
  else
    expect_platform_guidance "$OUT" "$RC" "$TOPIC"
    DETAIL="$WHY; $DETAIL"
  fi
  [ "$RESULT" = FAIL ] && show_output "$img non-TTY output (exit $RC)" "$OUT"
  add_row "$img" npm:non-tty "$libc" "$RESULT" "$NOTE$DETAIL"

  xrun "$cid" -e BWN_ALLOW_BOOTSTRAP=1 -- sh -lc 'bwn --version'
  first_out=$OUT first_rc=$RC
  judge "$first_out" "$first_rc" launcher
  # A second plain run must use the installed binary, not download again.
  xrun "$cid" -- sh -lc 'bwn --version'
  if grep -qi 'downloading' <<<"$OUT"; then RESULT=FAIL DETAIL="downloads again on the next run; $DETAIL"; fi
  [ "$RESULT" = FAIL ] && show_output "$img BWN_ALLOW_BOOTSTRAP=1 output (exit $first_rc)" "$first_out"
  boot_result=$RESULT
  add_row "$img" npm:bootstrap "$libc" "$RESULT" "$DETAIL$PULL_NOTE"
  if [ "$img" = "$EACCES_CELL" ]; then
    if [ "$EXPECT" = runs ] && [ "$boot_result" = PASS ]; then
      eacces_step "$cid" "$libc"
    else
      add_row "$img" npm:eacces "$libc" SKIP "needs a bootstrap that installs a working binary"
    fi
  fi
  docker rm -f "$cid" >/dev/null 2>&1

  # Container 2: a fresh install driven through a pseudo-terminal, the path
  # an interactive user takes (the launcher downloads without asking).
  cid=$(start_cell "$img" ${NET_ARGS[@]+"${NET_ARGS[@]}"} ${PKG_MOUNT[@]+"${PKG_MOUNT[@]}"}) ||
    { add_row "$img" npm:tty "$libc" SKIP "container did not start"; continue; }
  # Alpine has no script(1) until util-linux-misc is installed.
  xrun "$cid" -- sh -lc 'script --version 2>/dev/null | grep -q util-linux ||
    { ! command -v apk >/dev/null || apk add --no-cache -q util-linux-misc; } &&
    script --version 2>/dev/null | grep -q util-linux'
  if [ "$RC" -ne 0 ]; then
    add_row "$img" npm:tty "$libc" SKIP "no util-linux script(1) in the image: $(symptom "$OUT")"
  elif ! npm_install "$cid"; then
    add_row "$img" npm:tty "$libc" FAIL "npm i -g $INSTALL_SPEC failed: $(symptom "$OUT")"
  else
    xrun "$cid" -- sh -lc "script -qec 'bwn --version' /dev/null"
    judge "$OUT" "$RC" launcher
    [ "$RESULT" = FAIL ] && show_output "$img TTY output (exit $RC)" "$OUT"
    add_row "$img" npm:tty "$libc" "$RESULT" "$DETAIL"
  fi
  docker rm -f "$cid" >/dev/null 2>&1
done

# --------------------------------------------------------------- nvm cell
# Ubuntu 22.04, a non-root user, nvm and Node 22 installed the way nvm's
# README says, then bwn installed under nvm. Commands run in `bash -ic`,
# which reads ~/.bashrc like a new terminal does.
if selected "$NVM_CELL"; then
  log "== $NVM_CELL (nvm $NVM_VERSION, Node 22, non-root)"
  if ! pull ubuntu:22.04; then
    add_row "$NVM_CELL" nvm - SKIP "docker pull failed after 3 attempts"
  elif ! cid=$(start_cell ubuntu:22.04 ${NET_ARGS[@]+"${NET_ARGS[@]}"} ${PKG_MOUNT[@]+"${PKG_MOUNT[@]}"}); then
    add_row "$NVM_CELL" nvm - SKIP "container did not start"
  else
    libc=$(probe_libc "$cid")
    classify "$libc"
    # What a login terminal would have set; nvm's installer picks the
    # profile file to edit from SHELL.
    as_dev=(-u dev -w /home/dev -e HOME=/home/dev -e SHELL=/bin/bash)
    setup_err=''
    xrun "$cid" -- sh -lc 'apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends curl ca-certificates >/dev/null'
    [ "$RC" -eq 0 ] || setup_err="apt-get: $(symptom "$OUT")"
    if [ -z "$setup_err" ]; then
      xrun "$cid" -- sh -lc 'useradd -m -s /bin/bash dev'
      [ "$RC" -eq 0 ] || setup_err="useradd: $(symptom "$OUT")"
    fi
    if [ -z "$setup_err" ]; then
      xrun "$cid" "${as_dev[@]}" -- bash -lc "curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/$NVM_VERSION/install.sh | bash"
      [ "$RC" -eq 0 ] || setup_err="nvm install.sh: $(symptom "$OUT")"
    fi
    if [ -z "$setup_err" ]; then
      xrun "$cid" "${as_dev[@]}" -- bash -ic 'nvm install 22'
      [ "$RC" -eq 0 ] || setup_err="nvm install 22: $(symptom "$OUT")"
    fi
    if [ -z "$setup_err" ]; then
      # Make sure the install below really goes through nvm's npm.
      xrun "$cid" "${as_dev[@]}" -- bash -ic 'command -v npm'
      grep -q '^/home/dev/\.nvm/' <<<"$OUT" || setup_err="npm is not nvm's: $(symptom "$OUT")"
    fi
    if [ -n "$setup_err" ]; then
      # nvm or nodejs.org being unreachable says nothing about bwn.
      show_output "$NVM_CELL setup (exit $RC)" "$OUT"
      add_row "$NVM_CELL" nvm "$libc" SKIP "setup failed, $setup_err"
    else
      xrun "$cid" "${as_dev[@]}" -- bash -ic "npm i -g $INSTALL_SPEC"
      if [ "$RC" -ne 0 ]; then
        show_output "$NVM_CELL npm install (exit $RC)" "$OUT"
        add_row "$NVM_CELL" nvm:install "$libc" FAIL "npm i -g $INSTALL_SPEC under nvm failed: $(symptom "$OUT")"
      else
        xrun "$cid" "${as_dev[@]}" -- bash -ic true
        if grep -qi 'incompatible with nvm' <<<"$OUT"; then
          show_output "$NVM_CELL new shell" "$OUT"
          add_row "$NVM_CELL" nvm:shell "$libc" FAIL "a new shell prints the nvm prefix warning: $(grep -i -m1 'incompatible with nvm' <<<"$OUT")"
        else
          add_row "$NVM_CELL" nvm:shell "$libc" PASS "new shell has no \"incompatible with nvm\""
        fi
        xrun "$cid" "${as_dev[@]}" -- bash -ic "script -qec 'bwn --version' /dev/null"
        out=$(grep -vE 'cannot set terminal process group|no job control in this shell' <<<"$OUT")
        judge "$out" "$RC" launcher
        [ "$RESULT" = FAIL ] && show_output "$NVM_CELL TTY output (exit $RC)" "$out"
        add_row "$NVM_CELL" nvm:tty "$libc" "$RESULT" "$DETAIL"
      fi
    fi
    docker rm -f "$cid" >/dev/null 2>&1
  fi
fi

# ----------------------------------------------------------------- report
pass=0 fail=0 skip=0
for r in "${R_RESULT[@]}"; do
  case $r in PASS) pass=$((pass + 1)) ;; FAIL) fail=$((fail + 1)) ;; *) skip=$((skip + 1)) ;; esac
done
md_cell() { local s=${1//|/\\|}; printf '%s' "$s"; }
{
  echo "### Linux install matrix ($ARCH): buildwithnexus $VERSION from $SOURCE"
  echo
  echo "glibc floor: binary ${BIN_FLOOR:-unknown} (readelf), docs ${DOC_FLOOR:-none stated}, release.yml gate ${GATE_FLOOR:-not found} ($GATE_SRC)."
  echo
  echo "| cell | kind | libc | result | detail |"
  echo "|---|---|---|---|---|"
  for i in "${!R_CELL[@]}"; do
    printf '| %s | %s | %s | %s | %s |\n' "${R_CELL[$i]}" "${R_KIND[$i]}" "${R_LIBC[$i]}" \
      "${R_RESULT[$i]}" "$(md_cell "${R_DETAIL[$i]}")"
  done
  echo
  echo "$pass PASS, $fail FAIL, $skip SKIP"
} >"$WORK/report.md"
cat "$WORK/report.md"
[ -n "${GITHUB_STEP_SUMMARY:-}" ] && cat "$WORK/report.md" >>"$GITHUB_STEP_SUMMARY"

json_str() {
  local s=$1
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  printf '"%s"' "$s"
}
if [ -n "$JSON_OUT" ]; then
  {
    printf '{"version":%s,"arch":%s,"source":%s,' \
      "$(json_str "$VERSION")" "$(json_str "$ARCH")" "$(json_str "$SOURCE")"
    printf '"glibc_floor_binary":%s,"glibc_floor_docs":%s,"glibc_floor_gate":%s,' \
      "$(json_str "$BIN_FLOOR")" "$(json_str "$DOC_FLOOR")" "$(json_str "$GATE_FLOOR")"
    printf '"generated":%s,"pass":%d,"fail":%d,"skip":%d,"rows":[' \
      "$(json_str "$(date -u +%Y-%m-%dT%H:%M:%SZ)")" "$pass" "$fail" "$skip"
    for i in "${!R_CELL[@]}"; do
      [ "$i" -gt 0 ] && printf ','
      printf '{"cell":%s,"kind":%s,"libc":%s,"result":%s,"detail":%s}' \
        "$(json_str "${R_CELL[$i]}")" "$(json_str "${R_KIND[$i]}")" "$(json_str "${R_LIBC[$i]}")" \
        "$(json_str "${R_RESULT[$i]}")" "$(json_str "${R_DETAIL[$i]}")"
    done
    printf ']}\n'
  } >"$JSON_OUT"
fi

# All SKIP means nothing was tested; that is not a green run.
if [ $((pass + fail)) -eq 0 ]; then
  log "no cell produced a result"
  [ "$REPORT_ONLY" = 1 ] || exit 1
fi
if [ "$fail" -gt 0 ] && [ "$REPORT_ONLY" != 1 ]; then exit 1; fi
exit 0
