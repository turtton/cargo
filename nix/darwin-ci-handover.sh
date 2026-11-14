#!/usr/bin/env bash
# What `publish-darwin` hands to `verify-cache-darwin` (.github/workflows/nix.yml),
# as one JSON object on stdout (messages go to stderr):
#
#   nix/run-pinned.sh nix/darwin-ci-handover.sh result-cargo result-deps result-macho
#
# The arguments are the result links of `nix build .#cargo`, `.#cargo-deps` and
# `.#checks.aarch64-darwin.macho-config`. It runs in the job that built them,
# on the same runner and before any credential is placed there.
#
#   system             aarch64-darwin, as Nix of this runner says
#   commit             the commit that was built (GITHUB_SHA)
#   release            the content of TURTTON_VERSION
#   cargo, cargo-deps  the absolute store paths of the two roots
#   allowed            the other store paths of cargo's closure (space-separated):
#                      the outputs the Mach-O check lets the binary load
#   deployment-target  the macOS version the binary targets
#   macho              the allow lists of nix/check-macho.sh (flake.nix, JSON)
#
# Roots are exactly cargo and cargo-deps: no attic client is one of them.
# HANDOVER_GITHUB_OUTPUT=$GITHUB_OUTPUT also writes the values as step outputs.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=nix/pinned-env.sh
source "$here/pinned-env.sh"
# shellcheck source=nix/darwin-ci-lib.sh
source "$here/darwin-ci-lib.sh"

cargo_link="${1:?usage: darwin-ci-handover.sh CARGO-LINK DEPS-LINK MACHO-CONFIG-LINK}"
deps_link="${2:?usage: darwin-ci-handover.sh CARGO-LINK DEPS-LINK MACHO-CONFIG-LINK}"
macho_link="${3:?usage: darwin-ci-handover.sh CARGO-LINK DEPS-LINK MACHO-CONFIG-LINK}"

cargo="$(readlink -e -- "$cargo_link")" || fail_ci "$cargo_link does not exist"
deps="$(readlink -e -- "$deps_link")" || fail_ci "$deps_link does not exist"
macho_file="$(readlink -e -- "$macho_link")" || fail_ci "$macho_link does not exist"
for p in "$cargo" "$deps" "$macho_file"; do
  [[ "$p" =~ $store_path_re ]] || fail_ci "not an absolute store path: $p"
done

system="$(nix eval --raw --impure --expr builtins.currentSystem)"
[ "$system" = aarch64-darwin ] || fail_ci "the Nix system is $system, not aarch64-darwin"
[ "$(uname -m)" = arm64 ] || fail_ci "the runner is $(uname -m), not arm64"

commit="${GITHUB_SHA:?GITHUB_SHA}"
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || fail_ci "GITHUB_SHA is not a commit: $commit"
release="$(cat TURTTON_VERSION)"
[[ "$release" =~ ^[0-9A-Za-z.+-]+$ ]] || fail_ci "TURTTON_VERSION is not a release: $release"
if [ "${GITHUB_REF_TYPE:-}" = tag ]; then
  [ "${GITHUB_REF_NAME:-}" = "v$release" ] || fail_ci "the tag ${GITHUB_REF_NAME:-} is not v$release"
fi
[[ "$cargo" == /nix/store/*-turtton-cargo-"$release" ]] \
  || fail_ci "$cargo is not the package turtton-cargo-$release"
[[ "$deps" == /nix/store/*-turtton-cargo-deps-"$release" ]] \
  || fail_ci "$deps is not the dependencies turtton-cargo-deps-$release"

# The closure of the package is the package and the allowed outputs, nothing
# else (checks.<darwin>.closure fails otherwise).
closure="$(nix path-info --recursive "$cargo" | sort -u)"
rc=0
allowed="$(printf '%s\n' "$closure" | grep -vxF "$cargo")" || rc=$?
[ "$rc" -le 1 ] || fail_ci "grep failed on the closure of $cargo"
[ -n "$allowed" ] || fail_ci "the closure of $cargo has no other path: the analysis found nothing"
while IFS= read -r p; do
  [[ "$p" =~ $store_path_re ]] || fail_ci "not an absolute store path in the closure: $p"
  [ "$p" != "$deps" ] || fail_ci "cargo-deps is in the closure of cargo"
done <<EOF
$allowed
EOF

jq -e '
  (.minos | test("^[0-9]+\\.[0-9]+$"))
  and (.systemLibs | length > 0) and (.transitiveSystemLibs | length > 0)
  and (.requiredDeps | length > 0) and (.forbiddenDeps | length > 0)
  and ([.systemLibs[], .transitiveSystemLibs[]] | all(startswith("/")))
' "$macho_file" > /dev/null || fail_ci "$macho_file is not a usable allow list"
minos="$(jq -r .minos "$macho_file")" || fail_ci "could not read the deployment target of $macho_file"
allowed_line="$(printf '%s\n' "$allowed" | paste -sd' ' -)" || fail_ci "could not join the allowed paths"
[ -n "$allowed_line" ] || fail_ci "the allowed paths vanished"

handover="$(jq -n \
  --arg system "$system" \
  --arg commit "$commit" \
  --arg release "$release" \
  --arg cargo "$cargo" \
  --arg deps "$deps" \
  --arg allowed "$allowed_line" \
  --arg minos "$minos" \
  --slurpfile macho "$macho_file" \
  '{system: $system, commit: $commit, release: $release, cargo: $cargo, "cargo-deps": $deps,
    allowed: $allowed, "deployment-target": $minos, macho: $macho[0]}')"
printf '%s\n' "$handover"
# HANDOVER_GITHUB_OUTPUT: also append the values, one `name=value` line each
# (the macho object as one line of JSON), to this file, which is the
# GITHUB_OUTPUT of the step; they become the outputs of the publishing job.
if [ -n "${HANDOVER_GITHUB_OUTPUT:-}" ]; then
  printf '%s\n' "$handover" \
    | jq -r 'to_entries[] | "\(.key)=\(.value | if type == "string" then . else tojson end)"' >> "$HANDOVER_GITHUB_OUTPUT"
fi
