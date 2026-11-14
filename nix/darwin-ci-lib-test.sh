#!/usr/bin/env bash
# Tests of the helpers of nix/darwin-ci-lib.sh that the publish and verify
# scripts judge with, run under nix/run-pinned.sh in `build-darwin` (and on any
# machine with bash 3.2 or later and the usual tools):
#
#   nix/run-pinned.sh nix/darwin-ci-lib-test.sh
#
# A check is worth nothing if it passes when the query behind it failed, so the
# queries (`nix-store`, `nix` and `curl`) are shims that fail, answer nothing or
# answer something unexpected, and each case says whether the helper must
# accept or reject and, when it rejects, with which message. The case that
# prompted these tests is first: `nix-store` run inside the arguments of a
# `printf` failed without failing the `printf`, so two stores that both could
# not be queried described their paths with the same empty hash and references,
# and were found equal.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=nix/darwin-ci-lib.sh
source "$here/darwin-ci-lib.sh"

work="$(mktemp -d "${RUNNER_TEMP:-/tmp}/lib-test.XXXXXX")"
mkdir -p "$work/bin"
failed=0
passed=0

cat > "$work/bin/nix-store" << 'EOF'
#!/usr/bin/env bash
# nix-store [--store S] -q --hash|--references PATH
mode=""
for a in "$@"; do
  case "$a" in
    --hash) mode=hash ;;
    --references) mode=refs ;;
  esac
done
case "$mode" in
  hash)
    if [ -n "${SHIM_HASH_FAIL:-}" ]; then echo "error: simulated failure of nix-store --hash" >&2; exit 1; fi
    printf '%s\n' "${SHIM_HASH-sha256:0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa}"
    ;;
  refs)
    if [ -n "${SHIM_REFS_FAIL:-}" ]; then echo "error: simulated failure of nix-store --references" >&2; exit 1; fi
    printf '%s' "${SHIM_REFS-}"
    ;;
  *) exit 2 ;;
esac
EOF

cat > "$work/bin/nix" << 'EOF'
#!/usr/bin/env bash
# nix path-info --all --store S           (SHIM_LIST_FAIL, SHIM_LIST_OUT)
# nix path-info --option substituters "" P (SHIM_DAEMON: has | invalid | error)
[ "$1" = path-info ] || exit 2
shift
for a in "$@"; do
  if [ "$a" = --all ]; then
    if [ -n "${SHIM_LIST_FAIL:-}" ]; then echo "error: cannot connect to the store" >&2; exit 1; fi
    printf '%s' "${SHIM_LIST_OUT-}"
    exit 0
  fi
done
case "${SHIM_DAEMON:-has}" in
  has) exit 0 ;;
  invalid) echo "error: path '$*' is not valid" >&2; exit 1 ;;
  *) echo "error: cannot connect to socket at '/nix/var/nix/daemon-socket/socket'" >&2; exit 1 ;;
esac
EOF

cat > "$work/bin/curl" << 'EOF'
#!/usr/bin/env bash
# curl ... -o FILE -w FORMAT URL: writes SHIM_BODY to FILE, prints SHIM_CODE,
# and exits with SHIM_CURL_RC (default 0). The URL's hash is in $SHIM_LAST_URL.
out=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift ;;
  esac
  last="$1"
  shift
done
echo "$last" >> "${SHIM_URL_LOG:-/dev/null}"
[ -z "$out" ] || printf '%s' "${SHIM_BODY-}" > "$out"
if [ "${SHIM_CURL_RC:-0}" -ne 0 ]; then echo "curl: (6) Could not resolve host" >&2; exit "$SHIM_CURL_RC"; fi
printf '%s' "${SHIM_CODE:-404}"
EOF
chmod +x "$work/bin/nix-store" "$work/bin/nix" "$work/bin/curl"
PATH="$work/bin:$PATH"
export PATH

p1=/nix/store/0123456789abcdfghijklmnpqrsvwxyz-one-1.0
p2=/nix/store/abcdfghijklmnpqrsvwxyz0123456789-two-2.0
printf '%s\n%s\n' "$p1" "$p2" > "$work/list.txt"

# case NAME accept|reject PATTERN COMMAND...: runs the command in a subshell
# (fail_ci ends it); a rejection must print the pattern after \`::error::\`.
case_() {
  local name="$1" expect="$2" pattern="$3" status=0 log="$work/case-$1.log"
  shift 3
  ( "$@" ) > "$log" 2>&1 || status=$?
  if [ "$expect" = accept ] && [ "$status" -eq 0 ]; then
    passed=$((passed + 1))
    echo "ok (accepted): $name"
  elif [ "$expect" = reject ] && [ "$status" -ne 0 ] && grep -Eq "::error::.*($pattern)" "$log"; then
    passed=$((passed + 1))
    echo "ok (rejected): $name: $(grep -E '::error::' "$log" | head -n 1)"
  else
    failed=$((failed + 1))
    echo "FAILED: $name: expected $expect ($pattern), status $status"
    sed 's/^/    | /' "$log"
  fi
}

# --- describe_store: the hash and the references are two queries, both must work.
describe_ok() {
  describe_store "$work/d.txt" "$work/list.txt" --store "local?root=$work/s"
  cat "$work/d.txt"
}
case_ describe-valid accept - describe_ok
SHIM_REFS="$p2"$'\n'"$p1" case_ describe-sorted-references accept - bash -c "
  source '$here/darwin-ci-lib.sh'; describe_store '$work/d2.txt' '$work/list.txt' &&
  grep -qF ' sha256:0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa $p1,$p2' '$work/d2.txt'"
# A path without references is normal: the line ends with an empty field.
case_ describe-no-references accept - bash -c "
  source '$here/darwin-ci-lib.sh'; describe_store '$work/d3.txt' '$work/list.txt' &&
  grep -c -E '^/nix/store/[^ ]+ sha256:[0-9a-z]+ \$' '$work/d3.txt' | grep -qx 2"
SHIM_HASH_FAIL=1 case_ describe-hash-query-fails reject 'could not read the NAR hash' describe_ok
SHIM_REFS_FAIL=1 case_ describe-references-query-fails reject 'could not read the references' describe_ok
SHIM_HASH= case_ describe-empty-hash reject "unexpected NAR hash" describe_ok
SHIM_HASH=garbage case_ describe-garbage-hash reject "unexpected NAR hash" describe_ok
SHIM_HASH='sha256:' case_ describe-hash-without-digest reject "unexpected NAR hash" describe_ok
: > "$work/empty-list.txt"
case_ describe-empty-list reject 'lists no path' describe_store "$work/d4.txt" "$work/empty-list.txt"
printf 'not-a-path\n' > "$work/bad-list.txt"
case_ describe-bad-path reject 'not an absolute store path' describe_store "$work/d5.txt" "$work/bad-list.txt"

# --- compare_descriptions: the files that the old describe wrote when both
# stores could not be asked (an empty hash and references on every line) are
# identical, and must not be accepted as equal.
good_a="$p1 sha256:0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa $p2"
printf '%s\n' "$good_a" > "$work/c-a.txt"
printf '%s\n' "$good_a" > "$work/c-b.txt"
printf '%s\n' "$good_a" | sed 's/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/' > "$work/c-other.txt"
printf '%s  \n%s  \n' "$p1" "$p2" > "$work/c-empty-a.txt"
cp "$work/c-empty-a.txt" "$work/c-empty-b.txt"
case_ compare-equal accept - compare_descriptions "$work/c-a.txt" "$work/c-b.txt"
case_ compare-different-hash reject 'differ' compare_descriptions "$work/c-a.txt" "$work/c-other.txt"
case_ compare-identical-empty-hashes reject 'without a path and a hash' compare_descriptions "$work/c-empty-a.txt" "$work/c-empty-b.txt"
case_ compare-empty-file reject 'is empty' compare_descriptions "$work/c-a.txt" "$work/empty-list.txt"
case_ compare-missing-file reject 'is empty' compare_descriptions "$work/c-a.txt" "$work/does-not-exist.txt"

# --- assert_store_empty: a query that fails does not describe an empty store.
case_ store-empty accept - assert_store_empty "local?root=$work/s"
SHIM_LIST_FAIL=1 case_ store-listing-fails reject 'could not list' assert_store_empty "local?root=$work/s"
SHIM_LIST_OUT="$p1" case_ store-not-empty reject 'is not empty' assert_store_empty "local?root=$work/s"

# --- in_daemon: present, absent, and "could not ask" are three answers.
daemon_answer() {
  if in_daemon "$p1"; then echo present; else echo absent; fi
}
SHIM_DAEMON=has case_ daemon-has accept - bash -c "source '$here/darwin-ci-lib.sh'; in_daemon '$p1'"
SHIM_DAEMON=invalid case_ daemon-absent accept - bash -c "source '$here/darwin-ci-lib.sh'; ! in_daemon '$p1'"
SHIM_DAEMON=error case_ daemon-unreachable reject 'could not ask the daemon' daemon_answer

# --- expect_missing: only a missing path counts as missing.
printf "error: path '%s' is not valid\n" "$p1" > "$work/m-valid.log"
printf "error: unable to download 'https://x/y.narinfo': HTTP error 502\nerror: path '%s' is not valid\n" "$p1" > "$work/m-network.log"
printf 'error: something else happened\n' > "$work/m-other.log"
case_ missing-path accept - expect_missing "$work/m-valid.log" "$p1"
case_ missing-but-network-error reject 'network or credential error' expect_missing "$work/m-network.log" "$p1"
case_ failure-for-another-reason reject 'does not say that the path is missing' expect_missing "$work/m-other.log" "$p1"

# --- no_ignored_setting
printf "warning: ignoring the client-specified setting 'sandbox', because it is a restricted setting\n" > "$work/i-ignored.log"
printf 'copying 1 paths...\n' > "$work/i-fine.log"
case_ setting-honoured accept - no_ignored_setting "$work/i-fine.log"
case_ setting-ignored reject 'ignored a setting|ignored setting' no_ignored_setting "$work/i-ignored.log"

# --- probe_upstream: 404 is the only "not there"; everything else is an answer or an error.
UPSTREAM_URL=https://cache.example.invalid
export UPSTREAM_URL
PROBE_TMP="$work"
export PROBE_TMP
probe_is() {
  local want="$1" out
  out="$(probe_upstream "$p1")"
  case "$out" in
    "$want"*) echo "$out" ;;
    *) echo "::error::probe said '$out', expected $want"; exit 1 ;;
  esac
}
SHIM_CODE=404 case_ probe-404-is-fork accept - probe_is fork
SHIM_CODE=200 SHIM_BODY="StorePath: $p1" case_ probe-200-is-upstream accept - probe_is upstream
SHIM_CODE=200 SHIM_BODY="StorePath: $p2" case_ probe-200-other-path-is-error accept - probe_is error
SHIM_CODE=403 case_ probe-403-is-error accept - probe_is error
SHIM_CODE=500 case_ probe-500-is-error accept - probe_is error
SHIM_CURL_RC=6 case_ probe-no-connection-is-error accept - probe_is error
PROBE_TMP="$work/does-not-exist" SHIM_CODE=404 case_ probe-mktemp-fails-is-error accept - probe_is error
PROBE_TMP="$work"

# --- classify_closure: an error for one path fails the whole classification.
printf '%s\n%s\n' "$p1" "$p2" > "$work/closure.txt"
mkdir -p "$work/cl-ok" "$work/cl-bad" "$work/cl-mixed"
classify_ok() {
  classify_closure "$work/closure.txt" "$work/cl-ok"
  [ "$(wc -l < "$work/cl-ok/fork.txt")" -eq 2 ] && [ ! -s "$work/cl-ok/upstream.txt" ]
}
SHIM_CODE=404 case_ classify-all-fork accept - classify_ok
SHIM_CODE=502 case_ classify-error-fails reject 'could not ask' classify_closure "$work/closure.txt" "$work/cl-bad"
SHIM_CURL_RC=6 case_ classify-no-connection-fails reject 'could not ask' classify_closure "$work/closure.txt" "$work/cl-mixed"
case_ classify-empty-list reject 'lists no path' classify_closure "$work/empty-list.txt" "$work/cl-ok"

echo "$passed cases as expected, $failed not"
[ "$failed" -eq 0 ] || exit 1
[ "$passed" -gt 0 ] || exit 1
echo "darwin-ci-lib-test: OK"
