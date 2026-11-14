#!/usr/bin/env bash
# The second half of the pull-request rehearsal, on a fresh macOS runner whose
# Nix store has none of the paths:
#
#   PINNED_EXTRA_PACKAGES=python3 nix/run-pinned.sh nix/darwin-ci-rehearsal.sh DIR
#
# DIR is what nix/darwin-ci-rehearsal-export.sh made: handover.json, pub.key and
# the binary cache of the paths cache.nixos.org does not have. The cache is
# served on 127.0.0.1 and stands in for attic; the real verify script then runs
# its three steps (handover, fetch, daemon) against it exactly as
# `verify-cache-darwin` does against attic, with a made-up token. What a
# rehearsal cannot show is the network to attic and the real credential.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=nix/pinned-env.sh
source "$here/pinned-env.sh"
# shellcheck source=nix/darwin-ci-lib.sh
source "$here/darwin-ci-lib.sh"

dir="${1:?usage: darwin-ci-rehearsal.sh DIR}"
[[ "$(command -v python3)" == /nix/store/* ]] || fail_ci "python3 is not a pinned tool"
for f in handover.json pub.key cache/nix-cache-info; do
  [ -s "$dir/$f" ] || fail_ci "$dir/$f is missing"
done

work="$(mktemp -d "${RUNNER_TEMP:-/tmp}/rehearsal.XXXXXX")"
port=38417
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$dir/cache" > "$work/http.log" 2>&1 &
server=$!
trap 'kill "$server" 2> /dev/null || true' EXIT
for _ in $(seq 1 30); do
  if curl -fsS -o /dev/null "http://127.0.0.1:$port/nix-cache-info" 2> /dev/null; then
    break
  fi
  sleep 1
done
curl -fsS "http://127.0.0.1:$port/nix-cache-info"

# jq -e: a missing key (null) is a failure, not the string "null".
VC_SYSTEM="$(jq -er .system "$dir/handover.json")"
VC_COMMIT="$(jq -er .commit "$dir/handover.json")"
VC_RELEASE="$(jq -er .release "$dir/handover.json")"
VC_CARGO="$(jq -er .cargo "$dir/handover.json")"
VC_DEPS="$(jq -er '."cargo-deps"' "$dir/handover.json")"
VC_ALLOWED="$(jq -er .allowed "$dir/handover.json")"
VC_MINOS="$(jq -er '."deployment-target"' "$dir/handover.json")"
VC_MACHO="$(jq -ec .macho "$dir/handover.json")"
export VC_SYSTEM VC_COMMIT VC_RELEASE VC_CARGO VC_DEPS VC_ALLOWED VC_MINOS VC_MACHO
export ATTIC_URL="http://127.0.0.1:$port"
export ATTIC_HOST=127.0.0.1
ATTIC_KEY="$(cat "$dir/pub.key")"
[[ "$ATTIC_KEY" =~ ^[A-Za-z0-9._-]+:[A-Za-z0-9+/]+=*$ ]] || fail_ci "$dir/pub.key is not a public key: $ATTIC_KEY"
export ATTIC_KEY
export UPSTREAM_URL=https://cache.nixos.org
export UPSTREAM_KEY=cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=
export VC_WORK="$work/verify-cache"

echo "##[group]handover"
bash "$here/darwin-ci-verify-cache.sh" handover
echo "##[endgroup]"
echo "##[group]fetch"
ATTIC_TOKEN=rehearsal-token bash "$here/darwin-ci-verify-cache.sh" fetch
echo "##[endgroup]"
echo "##[group]daemon"
bash "$here/darwin-ci-verify-cache.sh" daemon
echo "##[endgroup]"

echo "== what the local cache was asked for (requests by status)"
awk '
  match($0, /" [0-9][0-9][0-9] /) { served[substr($0, RSTART + 2, 3)]++ }
  / code 404, message / { missing++ }
  END {
    for (code in served) print served[code], "responses with status", code
    print missing + 0, "404 error lines"
  }' "$work/http.log"
echo "rehearsal: OK"
