#!/usr/bin/env bash
# Records what the built package links and references: file type, otool -L and
# otool -l of the binary, its references, the closure and the sizes. It records
# first and judges nothing (the install check judges, inside the Nix sandbox),
# so a run whose check failed can still be read.
#
#   nix/darwin-ci-linkage.sh result-cargo result-deps
#
# Written for the bash 3.2 and BSD tools of a stock macOS.
set -eo pipefail

cargo_link="${1:-result-cargo}"
deps_link="${2:-result-deps}"
if [ ! -e "$cargo_link" ]; then
  echo "no $cargo_link: the package was not built"
  exit 0
fi
cargo_out="$(readlink "$cargo_link")"
bin="$cargo_out/bin/cargo"

echo "== $bin"
ls -l "$bin"
file "$bin"
lipo -archs "$bin" 2>&1 || true
echo "== otool -L"
otool -L "$bin"
echo "== otool -l"
otool -l "$bin"
echo "== LC_RPATH and the deployment target"
otool -l "$bin" | grep -E -A5 'cmd (LC_RPATH|LC_BUILD_VERSION|LC_VERSION_MIN_MACOSX)' || true

echo "== references of the package (what Nix found in its files)"
nix-store -q --references "$cargo_out" | sort
echo "== closure of the package"
nix path-info -r -s -S -h "$cargo_out"
echo "== libraries of the store that the binary loads, and what they load"
otool -L "$bin" | tail -n +2 | sed -E 's/^[[:space:]]+//; s/ \(compatibility version.*$//' | grep '^/nix/store/' | while IFS= read -r lib; do
  echo "-- $lib"
  otool -L "$lib"
done

total="$(nix path-info -S "$cargo_out" | awk '{print $2}')"
size="$(stat -f %z "$bin")"
refs="$(nix-store -q --references "$cargo_out" | wc -l | tr -d ' ')"
closure_paths="$(nix-store -qR "$cargo_out" | wc -l | tr -d ' ')"
deps_total=""
if [ -e "$deps_link" ]; then
  deps_total="$(nix path-info -S "$(readlink "$deps_link")" | awk '{print $2}')"
fi
echo "binary: $size bytes; closure of the package: $total bytes in $closure_paths paths; closure of the dependencies: ${deps_total:-unknown} bytes"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### build-darwin: package"
    echo
    echo "| | |"
    echo "|---|---|"
    echo "| binary | $size bytes |"
    echo "| references | $refs |"
    echo "| closure of the package | $total bytes, $closure_paths paths |"
    echo "| closure of the dependencies | ${deps_total:-unknown} bytes |"
    echo
    echo '```'
    otool -L "$bin"
    echo '```'
  } >> "$GITHUB_STEP_SUMMARY"
fi
