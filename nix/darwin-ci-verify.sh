#!/usr/bin/env bash
# Rehearsal of the signature checks of `verify-cache` (publish -> verify-cache in
# .github/workflows/nix.yml) for Darwin, with cache.nixos.org alone and no
# secret: the same commands, on a path of the public cache instead of one of
# attic.
#
#   1. take the output path of a plain, input-addressed package that the
#      daemon's store does not have, given by its absolute store path only;
#   2. fetch it from cache.nixos.org into an empty `local?root=` store and
#      verify it there;
#   3. `nix copy` it from that store to the daemon, trusting only another key
#      and requiring signatures: it must fail, for the missing signature and
#      for no other reason;
#   4. the same copy trusting the upstream key must succeed;
#   5. a made-up path must not be fetchable.
# --no-check-sigs and --substitute-on-destination are not used, and a warning
# that a setting was ignored fails the run.
#
# It runs under the bash and tools of the nixpkgs revision of flake.lock, not the
# runner's bash 3.2 and BSD tools: nix/run-pinned.sh nix/darwin-ci-verify.sh
set -eo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=nix/pinned-env.sh
source "$here/pinned-env.sh"
# shellcheck source=nix/darwin-ci-lib.sh
source "$here/darwin-ci-lib.sh"

UPSTREAM_URL=https://cache.nixos.org
UPSTREAM_KEY=cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=
work="$(mktemp -d "${RUNNER_TEMP:-/tmp}/verify-rehearsal.XXXXXX")"
trap 'chmod -R u+w "$work" 2> /dev/null; rm -rf "$work"' EXIT

echo "== 1. a package of cache.nixos.org that the daemon does not have"
# The nixpkgs the package is built from, by its locked revision.
path="$(nix eval --raw "github:NixOS/nixpkgs/$NIXPKGS_REV#legacyPackages.aarch64-darwin.hello.outPath")"
echo "path: $path"
case "$path" in
  /nix/store/????????????????????????????????-hello-*) ;;
  *) echo "::error::unexpected store path: $path"; exit 1 ;;
esac
if in_daemon "$path"; then
  fail_ci "the daemon already has $path"
fi
echo "the daemon does not have it (as intended)"
# Input-addressed: a content-addressed path would carry no signature check of
# this kind. `nix path-info --json` on the cache reports it.
hash="${path#/nix/store/}"
hash="${hash%%-*}"
curl -fsS "$UPSTREAM_URL/$hash.narinfo" > "$work/hello.narinfo"
cat "$work/hello.narinfo"
grep -qxF "StorePath: $path" "$work/hello.narinfo"
grep -q '^Sig: cache.nixos.org-1:' "$work/hello.narinfo"
if grep -q '^CA: ' "$work/hello.narinfo"; then
  echo "::error::$path is content-addressed"
  exit 1
fi

echo "== 2. fetch it into an empty local?root store and verify it there"
store="$work/store"
mkdir -p "$store"
assert_store_empty "local?root=$store"
nix copy --from "$UPSTREAM_URL" --to "local?root=$store" \
  --option trusted-public-keys "$UPSTREAM_KEY" "$path" 2>&1 | tee "$work/fetch.log"
no_ignored_setting "$work/fetch.log"
nix path-info --store "local?root=$store" "$path"
nix store verify --store "local?root=$store" --option trusted-public-keys "$UPSTREAM_KEY" "$path"
echo "the fetched path is intact and signed by a trusted key"

echo "== 3. copy to the daemon trusting another key only: must fail for the missing signature"
nix key generate-secret --key-name rehearsal-other-1 > "$work/other.sec"
other_key="$(nix key convert-secret-to-public < "$work/other.sec")"
echo "other key: $other_key"
if nix copy --from "local?root=$store" --to daemon \
  --option require-sigs true --option substituters "" \
  --option trusted-public-keys "$other_key" "$path" > "$work/negative.log" 2>&1; then
  cat "$work/negative.log"
  echo "::error::the copy succeeded without a trusted signature"
  exit 1
fi
cat "$work/negative.log"
no_ignored_setting "$work/negative.log"
grep -Eiq 'lacks a (valid )?signature' "$work/negative.log" || {
  echo "::error::the copy failed, but not for a missing signature"
  exit 1
}
if in_daemon "$path"; then
  fail_ci "the daemon has the path after the failed copy"
fi
echo "failed as expected: the signature is missing"

echo "== 4. the same copy trusting the upstream key must succeed"
nix copy --from "local?root=$store" --to daemon \
  --option require-sigs true --option substituters "" \
  --option trusted-public-keys "$UPSTREAM_KEY" "$path" 2>&1 | tee "$work/positive.log"
no_ignored_setting "$work/positive.log"
nix path-info --option substituters "" "$path"

echo "== 5. a made-up path must not be fetchable"
fake=/nix/store/00000000000000000000000000000000-turtton-cargo-missing
fake_store="$work/fake-store"
mkdir -p "$fake_store"
if nix copy --from "$UPSTREAM_URL" --to "local?root=$fake_store" \
  --option trusted-public-keys "$UPSTREAM_KEY" "$fake" > "$work/fake.log" 2>&1; then
  cat "$work/fake.log"
  echo "::error::a made-up path was fetched"
  exit 1
fi
cat "$work/fake.log"
no_ignored_setting "$work/fake.log"
expect_missing "$work/fake.log" "$fake"
echo "failed as expected"

echo "optional step (cargo-deps expanded into a local?root store and verified) is not done: its paths carry no signature, and copying them without one needs --no-check-sigs"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### build-darwin: verify rehearsal (cache.nixos.org only)"
    echo
    echo "Path: \`$path\`"
    echo
    echo "- fetched into an empty \`local?root\` store and verified: ok"
    echo "- copy to the daemon trusting only another key, \`require-sigs true\`: failed for the missing signature, as expected"
    echo "- copy trusting the upstream key: ok"
    echo "- a made-up path: not fetchable, as expected"
  } >> "$GITHUB_STEP_SUMMARY"
fi
