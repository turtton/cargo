#!/usr/bin/env bash
# The first half of the pull-request rehearsal of `publish-darwin` and
# `verify-cache-darwin`, in the job that has just built the package:
#
#   nix/run-pinned.sh nix/darwin-ci-rehearsal-export.sh \
#     ./result-cargo ./result-deps ./result-macho DIR
#
# It makes what `publish-darwin` hands over (DIR/handover.json) and what attic
# would hold: a binary cache DIR/cache of the paths of the two closures that
# cache.nixos.org does not have, signed with a key made for this run (the
# public key is DIR/pub.key; the secret key stays outside DIR). A fresh runner
# then verifies it with the real verify script (nix/darwin-ci-rehearsal.sh),
# the store of which has none of these paths.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=nix/pinned-env.sh
source "$here/pinned-env.sh"
# shellcheck source=nix/darwin-ci-lib.sh
source "$here/darwin-ci-lib.sh"

cargo_link="${1:?usage: darwin-ci-rehearsal-export.sh CARGO DEPS MACHO-CONFIG DIR}"
deps_link="${2:?usage: darwin-ci-rehearsal-export.sh CARGO DEPS MACHO-CONFIG DIR}"
macho_link="${3:?usage: darwin-ci-rehearsal-export.sh CARGO DEPS MACHO-CONFIG DIR}"
out="${4:?usage: darwin-ci-rehearsal-export.sh CARGO DEPS MACHO-CONFIG DIR}"
UPSTREAM_URL="${UPSTREAM_URL:-https://cache.nixos.org}"

work="$(mktemp -d "${RUNNER_TEMP:-/tmp}/rehearsal-export.XXXXXX")"
mkdir -p "$out/cache"

bash "$here/darwin-ci-handover.sh" "$cargo_link" "$deps_link" "$macho_link" > "$out/handover.json"
jq . "$out/handover.json"
cargo="$(jq -er .cargo "$out/handover.json")"
deps="$(jq -er '."cargo-deps"' "$out/handover.json")"
[[ "$cargo" =~ $store_path_re ]] || fail_ci "not an absolute store path: $cargo"
[[ "$deps" =~ $store_path_re ]] || fail_ci "not an absolute store path: $deps"

nix path-info --recursive "$cargo" "$deps" | sort -u > "$work/closure.txt"
echo "closure: $(wc -l < "$work/closure.txt") paths"
classify_closure "$work/closure.txt" "$work"
echo "cache.nixos.org has $(wc -l < "$work/upstream.txt") of them; the cache below gets the other $(wc -l < "$work/fork.txt"):"
cat "$work/fork.txt"
grep -qxF "$cargo" "$work/fork.txt" || fail_ci "$cargo is in $UPSTREAM_URL"
grep -qxF "$deps" "$work/fork.txt" || fail_ci "$deps is in $UPSTREAM_URL"

nix key generate-secret --key-name rehearsal-attic-1 > "$work/rehearsal.sec"
nix key convert-secret-to-public < "$work/rehearsal.sec" > "$out/pub.key"
mapfile -t fork < "$work/fork.txt"
# Nix refuses to add a path to a binary cache whose references the cache does
# not have, so both closures go into a cache of this script first; the cache
# that attic stands in for gets the narinfo and the NAR of the paths of attic's
# only, as the attic client uploads.
nix copy --to "file://$work/full-cache?secret-key=$work/rehearsal.sec&compression=zstd" "$cargo" "$deps"
cp "$work/full-cache/nix-cache-info" "$out/cache/nix-cache-info"
for p in "${fork[@]}"; do
  name="${p#/nix/store/}"
  hash="${name%%-*}"
  url="$(sed -n 's/^URL: //p' "$work/full-cache/$hash.narinfo")"
  [[ "$url" =~ ^nar/[0-9a-z]+\.nar\.zst$ ]] || fail_ci "unexpected NAR location in the narinfo of $p: $url"
  mkdir -p "$out/cache/nar"
  cp "$work/full-cache/$hash.narinfo" "$out/cache/$hash.narinfo"
  cp "$work/full-cache/$url" "$out/cache/$url"
done
narinfos="$(find "$out/cache" -name '*.narinfo' | wc -l)"
[ "$narinfos" -eq "${#fork[@]}" ] || fail_ci "the cache has $narinfos narinfo files for ${#fork[@]} paths"
du -sh "$out/cache"
echo "public key: $(cat "$out/pub.key")"
