# shellcheck shell=bash
# Sourced by the Darwin CI scripts that must not depend on the bash 3.2 and the
# BSD tools of a stock macOS runner. They run under nix/run-pinned.sh, which
# starts them with the bash and the GNU tools of the nixpkgs revision locked in
# flake.lock (`nix shell github:NixOS/nixpkgs/<rev>#bash ...`).
#
# This file refuses to go on unless that is the case: the shell is a store
# bash, the revision and the directory of the runner's own nix are known, PATH
# holds the store tools and the runner's nix and nothing else, and every tool
# the scripts use resolves into the store (the runner's nix excepted).
#
#   NIXPKGS_REV     the locked nixpkgs revision the tools come from
#   PINNED_NIX_BIN  the directory of the runner's nix (the client of its daemon)

case "${BASH:-}" in
  /nix/store/*) ;;
  *)
    echo "::error::run this script with the pinned bash (nix/run-pinned.sh), not ${BASH:-unknown}"
    exit 1
    ;;
esac
: "${NIXPKGS_REV:?the nixpkgs revision of flake.lock (nix/run-pinned.sh sets it)}"
: "${PINNED_NIX_BIN:?the directory of the nix of the runner (nix/run-pinned.sh sets it)}"
[[ "$NIXPKGS_REV" =~ ^[0-9a-f]{40}$ ]] || {
  echo "::error::NIXPKGS_REV is not a commit: $NIXPKGS_REV"
  exit 1
}

# Only the pinned tools and the runner's nix.
pinned_path="$(printf '%s' "$PATH" | tr ':' '\n' | grep '^/nix/store/' | paste -sd: -)" || pinned_path=""
[ -n "$pinned_path" ] || {
  echo "::error::PATH holds no store directory: this is not nix shell of nix/run-pinned.sh"
  exit 1
}
PATH="$pinned_path:$PINNED_NIX_BIN"
export PATH

for pinned_tool in awk basename bash cat chmod comm cmp cp curl cut date dirname env find grep head jq mkdir mktemp otool paste readlink rm sed sleep sort stat tail tee timeout tr uname uniq wc xargs; do
  pinned_found="$(command -v "$pinned_tool" || true)"
  case "$pinned_found" in
    /nix/store/*) ;;
    *)
      echo "::error::$pinned_tool is not a pinned tool: '$pinned_found'"
      exit 1
      ;;
  esac
done
for runner_tool in nix nix-store; do
  [ "$(command -v "$runner_tool" || true)" = "$PINNED_NIX_BIN/$runner_tool" ] || {
    echo "::error::$runner_tool is not the runner's: '$(command -v "$runner_tool" || true)'"
    exit 1
  }
done
unset pinned_path pinned_tool pinned_found runner_tool
