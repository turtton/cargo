#!/usr/bin/env bash
# Runs a script under the bash and the GNU tools of the nixpkgs revision locked
# in flake.lock, not the runner's bash 3.2 and BSD tools:
#
#   nix/run-pinned.sh nix/darwin-ci-verify-cache.sh fetch
#
# The packages are fetched from cache.nixos.org by that revision directly (not
# through `--inputs-from`, which would follow whatever the flake resolves).
# PINNED_EXTRA_PACKAGES adds nixpkgs attributes, e.g. "python3". The script
# starts with `source nix/pinned-env.sh`, which refuses a run outside this one.
#
# This launcher itself is the only part written for the runner's bash 3.2.
set -eo pipefail

if [ "$#" -lt 1 ]; then
  echo "usage: nix/run-pinned.sh SCRIPT [ARGUMENT...]" >&2
  exit 2
fi

rev="$(nix eval --raw --impure --expr '(builtins.fromJSON (builtins.readFile ./flake.lock)).nodes.nixpkgs.locked.rev')"
case "$rev" in
  *[!0-9a-f]* | "")
    echo "::error::not a nixpkgs revision: '$rev'" >&2
    exit 1
    ;;
esac
if [ "${#rev}" -ne 40 ]; then
  echo "::error::not a nixpkgs revision: '$rev'" >&2
  exit 1
fi
echo "pinned nixpkgs: $rev" >&2

refs=()
for name in bash coreutils diffutils gnugrep gnused gawk findutils curl jq cctools ${PINNED_EXTRA_PACKAGES:-}; do
  refs+=("github:NixOS/nixpkgs/$rev#$name")
done

NIXPKGS_REV="$rev" PINNED_NIX_BIN="$(dirname "$(command -v nix)")" \
  exec nix shell "${refs[@]}" -c bash "$@"
