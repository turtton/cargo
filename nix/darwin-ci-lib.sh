# shellcheck shell=bash
# Functions shared by the Darwin publish and verify scripts. Source it after
# nix/pinned-env.sh.

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
  body="$(mktemp "${PROBE_TMP:?}/narinfo.XXXXXX")"
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
  [ "$(wc -l < "$dir/classified.txt")" -eq "$(sort -u "$list" | wc -l)" ] \
    || fail_ci "the classification does not cover every path"
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
