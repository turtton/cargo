# shellcheck shell=bash
# Functions shared by the Darwin publish and verify scripts. Source it after
# nix/pinned-env.sh (the tests, nix/darwin-ci-lib-test.sh, source it alone with
# shims for nix, nix-store and curl).
#
# A command substitution whose command fails does not fail the command it is an
# argument of (`printf '%s' "$(false)"` succeeds), and a pipeline's status is
# its last command's unless pipefail is set. A check built on that passes when
# the query behind it failed. So here every query is its own command, its status
# is tested, and what it printed is validated before it is used; the callers
# follow the same rule (nix/darwin-ci-lib-test.sh fails the queries to prove it).

fail_ci() {
  echo "::error::$*"
  exit 1
}

# Fails when Nix says that a setting of ours was ignored (e.g. because the user
# is not trusted): the check would then have run with other settings.
no_ignored_setting() {
  if grep -Eiq 'ignoring the (client|user)-specified setting|unknown setting' "$1"; then
    echo "::error::Nix reported an ignored setting:"
    grep -Ei 'ignoring the (client|user)-specified setting|unknown setting' "$1"
    exit 1
  fi
}

# [[ ]] pattern of an absolute store path.
# shellcheck disable=SC2034
store_path_re='^/nix/store/[0-9a-df-np-sv-z]{32}-[A-Za-z0-9+._?=-]+$'

# Succeeds when the daemon's store has the path $1, fails with status 1 when it
# says that the path is not valid, and ends the script for any other answer:
# `nix path-info` failing because the daemon is unreachable is not proof that
# the path is absent. Substituters are off, so only the store itself answers.
in_daemon() {
  local out
  if out="$(nix path-info --option substituters "" "$1" 2>&1)"; then
    return 0
  fi
  case "$out" in
    *'is not valid'*) return 1 ;;
    *) fail_ci "could not ask the daemon about $1: $out" ;;
  esac
}

# assert_store_empty STORE-URI: the store lists no path. A failing `nix
# path-info` is not an empty store.
assert_store_empty() {
  local out
  out="$(nix path-info --all --store "$1" 2>&1)" || fail_ci "could not list the paths of $1: $out"
  [ -z "$out" ] || fail_ci "$1 is not empty: $out"
}

# describe_store OUTPUT-FILE LIST-FILE [nix-store option...]: writes one line
# `PATH HASH REFERENCES` for each path of LIST-FILE, as the store given by the
# options (none: the daemon) reports it: the NAR hash (`sha256:...`, never
# empty) and the sorted, comma-separated references (empty when there are none).
# The hash and the references are two commands, each of which must succeed.
describe_store() {
  local out="$1" list="$2" p hash refs
  shift 2
  : > "$out"
  [ -s "$list" ] || fail_ci "$list lists no path"
  while IFS= read -r p; do
    [[ "$p" =~ $store_path_re ]] || fail_ci "not an absolute store path in $list: $p"
    hash="$(nix-store "$@" -q --hash "$p" < /dev/null)" || fail_ci "could not read the NAR hash of $p"
    [[ "$hash" =~ ^sha256:[0-9a-z]+$ ]] || fail_ci "unexpected NAR hash of $p: '$hash'"
    refs="$(nix-store "$@" -q --references "$p" < /dev/null)" || fail_ci "could not read the references of $p"
    if [ -n "$refs" ]; then
      refs="$(printf '%s\n' "$refs" | sort | paste -sd, -)" || fail_ci "could not sort the references of $p"
      [ -n "$refs" ] || fail_ci "the references of $p vanished"
    fi
    printf '%s %s %s\n' "$p" "$hash" "$refs" >> "$out"
  done < "$list"
  [ "$(wc -l < "$out")" -eq "$(wc -l < "$list")" ] || fail_ci "not every path of $list has a description"
}

# compare_descriptions A B: the two files describe the same paths in the same
# way, and every line has a hash.
compare_descriptions() {
  local f total good rc
  for f in "$1" "$2"; do
    [ -s "$f" ] || fail_ci "$f is empty"
    total="$(wc -l < "$f")" || fail_ci "could not count the lines of $f"
    rc=0
    good="$(grep -cE '^/nix/store/[0-9a-df-np-sv-z]{32}-[A-Za-z0-9+._?=-]+ sha256:[0-9a-z]+ [^ ]*$' "$f")" || rc=$?
    [ "$rc" -le 1 ] || fail_ci "grep failed on $f"
    [ "$good" -eq "$total" ] || fail_ci "$f has a line without a path and a hash"
  done
  cmp "$1" "$2" || fail_ci "the descriptions in $1 and $2 differ"
}

# What Nix says of a path that the cache does not have depends on its version
# ("is not valid", or that no substituter can build the path); an error of
# the network, the TLS handshake or the credential is no proof of absence.
expect_missing() {
  grep -Eq 'is not valid|no substituter that can build it|know how to build' "$1" \
    || fail_ci "the fetch of $2 failed, but Nix does not say that the path is missing"
  if grep -Eiq 'unable to download|HTTP error|could not resolve|connection (refused|reset)|timed out|certificate|unauthori[sz]ed|forbidden' "$1"; then
    fail_ci "the fetch of $2 failed with a network or credential error, not for a missing path"
  fi
}

# Asks cache.nixos.org ($UPSTREAM_URL) about the store path $1 by its narinfo
# and prints `upstream PATH`, `fork PATH` or `error PATH: reason`. `nix
# path-info` says "is not valid" for an HTTP 401/403 as well as for a 404, so
# only the status can tell a path the cache does not have from a query that
# failed. Only a 404 means "not there"; a 200 must be the narinfo of this very
# path; every other answer (403, 5xx, no connection) is an error.
probe_upstream() {
  local path="$1" name hash body code err
  name="${path#/nix/store/}"
  hash="${name%%-*}"
  if ! body="$(mktemp "${PROBE_TMP:?}/narinfo.XXXXXX")"; then
    printf 'error %s: mktemp failed\n' "$path"
    return 0
  fi
  if ! code="$(curl -sS --retry 5 --retry-delay 2 --retry-all-errors --connect-timeout 10 --max-time 60 \
    -o "$body" -w '%{http_code}' "$UPSTREAM_URL/$hash.narinfo" 2> "$body.err")"; then
    err="$(tr '\n' ' ' < "$body.err")"
    printf 'error %s: %s\n' "$path" "$err"
  elif [ "$code" = 404 ]; then
    printf 'fork %s\n' "$path"
  elif [ "$code" = 200 ] && grep -qxF "StorePath: $path" "$body"; then
    printf 'upstream %s\n' "$path"
  else
    printf 'error %s: HTTP %s from %s/%s.narinfo\n' "$path" "$code" "$UPSTREAM_URL" "$hash"
  fi
  rm -f "$body" "$body.err"
}
export -f probe_upstream

# classify_closure LIST DIR: classifies every path of the file LIST and writes
# DIR/classified.txt, DIR/upstream.txt (cache.nixos.org has them) and
# DIR/fork.txt (only attic can have them, sorted). Any error fails.
classify_closure() {
  local list="$1" dir="$2"
  export UPSTREAM_URL
  PROBE_TMP="$dir"
  export PROBE_TMP
  # The inner shell expands $1, not this one.
  # shellcheck disable=SC2016
  sort -u "$list" | xargs -P 16 -I{} bash -c 'probe_upstream "$1"' _ {} > "$dir/classified.txt"
  if grep -q '^error ' "$dir/classified.txt"; then
    grep '^error ' "$dir/classified.txt" >&2
    fail_ci "could not ask $UPSTREAM_URL about every path"
  fi
  local expected got
  expected="$(sort -u "$list" | wc -l)" || fail_ci "could not count the paths of $list"
  got="$(wc -l < "$dir/classified.txt")" || fail_ci "could not count the classified paths"
  [ "$expected" -gt 0 ] || fail_ci "$list lists no path"
  [ "$got" -eq "$expected" ] || fail_ci "the classification covers $got of $expected paths"
  [ "$(grep -c -E '^(upstream|fork) ' "$dir/classified.txt")" -eq "$expected" ] \
    || fail_ci "a classification line is neither upstream nor fork"
  sed -n 's/^upstream //p' "$dir/classified.txt" | sort > "$dir/upstream.txt"
  sed -n 's/^fork //p' "$dir/classified.txt" | sort > "$dir/fork.txt"
}

# Appends stdin to the job summary when there is one.
summary() {
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    cat >> "$GITHUB_STEP_SUMMARY"
  else
    cat > /dev/null
  fi
}
