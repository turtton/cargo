#!/usr/bin/env bash
# Dry run of ci/push-attic.sh on a Darwin runner, with a stub in place of the
# attic client, on the packages that were just built. It is what `publish-darwin`
# will run, minus the network:
#
#   nix/run-pinned.sh nix/darwin-ci-push-dryrun.sh ./result-cargo ./result-deps
#
# It shows that the script, under the GNU tools of the pinned nixpkgs (readlink
# -e and timeout --kill-after do not exist on stock macOS), pushes the closure
# of each root, one NAR stream at a time (`push --stdin --jobs 1 home`), and
# nothing else; retries a failed push and sleeps in between; gives up after its
# attempts with a failure; ends a push that hangs at its limit; and refuses an
# argument that is not a store path before it pushes anything.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=nix/pinned-env.sh
source "$here/pinned-env.sh"
# shellcheck source=nix/darwin-ci-lib.sh
source "$here/darwin-ci-lib.sh"

cargo_link="${1:?usage: darwin-ci-push-dryrun.sh CARGO-LINK DEPS-LINK}"
deps_link="${2:?usage: darwin-ci-push-dryrun.sh CARGO-LINK DEPS-LINK}"
cargo="$(readlink -e -- "$cargo_link")"
deps="$(readlink -e -- "$deps_link")"
repo="$(cd "$here/.." && pwd)"

work="$(mktemp -d "${RUNNER_TEMP:-/tmp}/push-dryrun.XXXXXX")"
real_sleep="$(command -v sleep)"
mkdir -p "$work/shim"

# The attic client: records its arguments and standard input, and fails or hangs
# on request.
cat > "$work/attic" << 'EOF'
#!/usr/bin/env bash
set -euo pipefail
state="${ATTIC_STUB_DIR:?}"
n=$(($(cat "$state/count" 2> /dev/null || echo 0) + 1))
echo "$n" > "$state/count"
printf '%s\n' "$*" > "$state/args.$n"
cat > "$state/stdin.$n"
if [ "$n" -le "${ATTIC_STUB_FAIL_FIRST:-0}" ]; then
  echo "stub attic: failing call $n" >&2
  exit 1
fi
if [ -n "${ATTIC_STUB_SLEEP:-}" ]; then
  "${ATTIC_STUB_REAL_SLEEP:?}" "$ATTIC_STUB_SLEEP"
fi
exit 0
EOF
chmod +x "$work/attic"

# A sleep that records its argument and returns at once: the retry delay is
# 60 s or more.
cat > "$work/shim/sleep" << 'EOF'
#!/usr/bin/env bash
echo "$*" >> "${ATTIC_STUB_DIR:?}/sleeps"
EOF
chmod +x "$work/shim/sleep"

# run_push NAME ARGS...: runs ci/push-attic.sh with the stub; the status is
# left in $push_status, the output in $work/NAME.log.
run_push() {
  local name="$1"
  shift
  mkdir -p "$work/$name"
  push_status=0
  (
    cd "$repo"
    ATTIC_BIN="$work/attic" ATTIC_STUB_DIR="$work/$name" ATTIC_STUB_REAL_SLEEP="$real_sleep" \
      PATH="$work/shim:$PATH" bash ci/push-attic.sh "$@"
  ) > "$work/$name.log" 2>&1 || push_status=$?
  cat "$work/$name.log"
}

echo "== 1. both roots are pushed, their closures only, one stream each"
run_push ok "$cargo_link" "$deps_link"
[ "$push_status" -eq 0 ] || fail_ci "the push failed with the stub: $push_status"
[ "$(cat "$work/ok/count")" -eq 2 ] || fail_ci "expected two pushes, one per root"
for n in 1 2; do
  [ "$(cat "$work/ok/args.$n")" = "push --stdin --jobs 1 home" ] || fail_ci "unexpected arguments: $(cat "$work/ok/args.$n")"
done
nix path-info --recursive "$cargo" > "$work/expected.1"
nix path-info --recursive "$deps" > "$work/expected.2"
for n in 1 2; do
  sort "$work/expected.$n" > "$work/expected-sorted.$n"
  sort "$work/ok/stdin.$n" > "$work/pushed-sorted.$n"
  cmp "$work/expected-sorted.$n" "$work/pushed-sorted.$n" || fail_ci "push $n is not the closure of its root"
done
grep -qxF "$cargo" "$work/ok/stdin.1" || fail_ci "the package is not in the first push"
grep -qxF "$deps" "$work/ok/stdin.2" || fail_ci "the dependencies are not in the second push"
[ ! -e "$work/ok/sleeps" ] || fail_ci "a push that worked slept"
echo "ok: pushed $(wc -l < "$work/ok/stdin.1") and $(wc -l < "$work/ok/stdin.2") paths"

echo "== 2. a failed push is retried after a delay"
ATTIC_PUSH_ATTEMPTS=2 ATTIC_STUB_FAIL_FIRST=1 run_push retry "$cargo_link" "$deps_link"
[ "$push_status" -eq 0 ] || fail_ci "the retry did not recover: $push_status"
[ "$(cat "$work/retry/count")" -eq 3 ] || fail_ci "expected three calls: fail, retry, second root"
[ "$(wc -l < "$work/retry/sleeps")" -eq 1 ] || fail_ci "expected one delay"
delay="$(cat "$work/retry/sleeps")"
{ [ "$delay" -ge 60 ] && [ "$delay" -le 119 ]; } || fail_ci "unexpected delay: $delay"
grep -q 'retrying in' "$work/retry.log" || fail_ci "no retry message"

echo "== 3. it fails after its attempts, and still tries the other root"
ATTIC_PUSH_ATTEMPTS=2 ATTIC_STUB_FAIL_FIRST=100 run_push giveup "$cargo_link" "$deps_link"
[ "$push_status" -eq 1 ] || fail_ci "expected a failure, got $push_status"
[ "$(cat "$work/giveup/count")" -eq 4 ] || fail_ci "expected two attempts per root"
[ "$(grep -c 'failed after 2 attempts' "$work/giveup.log")" -eq 2 ] || fail_ci "expected one message per root"

echo "== 4. a push that hangs is ended at its limit"
start=$SECONDS
ATTIC_PUSH_ATTEMPTS=1 ATTIC_PUSH_TIMEOUT=2s ATTIC_STUB_SLEEP=120 run_push hang "$cargo_link" "$deps_link"
elapsed=$((SECONDS - start))
echo "elapsed: ${elapsed}s"
[ "$push_status" -eq 1 ] || fail_ci "expected a failure, got $push_status"
[ "$elapsed" -lt 60 ] || fail_ci "the hanging push was not ended in time: ${elapsed}s"
[ "$(cat "$work/hang/count")" -eq 2 ] || fail_ci "expected one attempt per root"

echo "== 5. an argument that is not a store path is refused before any push"
run_push missing "$cargo_link" ./does-not-exist
[ "$push_status" -eq 1 ] || fail_ci "a missing link was accepted"
[ ! -e "$work/missing/count" ] || fail_ci "something was pushed before the arguments were checked"
run_push outside "$cargo_link" "$work"
[ "$push_status" -eq 1 ] || fail_ci "a path outside the store was accepted"
[ ! -e "$work/outside/count" ] || fail_ci "something was pushed before the arguments were checked"

{
  echo "### build-darwin: dry run of ci/push-attic.sh (stub client)"
  echo
  echo "- both roots pushed as \`push --stdin --jobs 1 home\`, each with its closure only"
  echo "- retry with a delay of $delay s, failure after the attempts, a hanging push ended at its limit, bad arguments refused"
} | summary
echo "push-attic dry run: OK"
