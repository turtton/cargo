#!/usr/bin/env bash
# Pushes the closures of the given store paths (result links) to the attic
# cache `home`. Used by .github/workflows/nix.yml; needs an attic client that
# is logged in (`attic login --set-default home ...`).
#
#   ci/push-attic.sh result-attic result-cargo result-deps
#
# ATTIC_BIN         attic client (default: ./result-attic/bin/attic, the patched
#                   client of `nix build .#attic-client`)
# ATTIC_PUSH_ATTEMPTS  tries per path (default 4)
# ATTIC_PUSH_TIMEOUT   limit of one try (default 20m)
set -euo pipefail

attic="${ATTIC_BIN:-$PWD/result-attic/bin/attic}"
attempts="${ATTIC_PUSH_ATTEMPTS:-4}"
attempt_timeout="${ATTIC_PUSH_TIMEOUT:-20m}"
status=0

# Upload closures so the dependencies, the vendored sources and the package are
# reusable. One NAR at a time (--jobs 1) limits the load on the server, and the
# patched client tolerates its slow storage.
for result in "$@"; do
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    # Bound the whole pipeline, path-info included; the kill grace keeps a try
    # within its limit even if termination is ignored. The positional
    # parameters expand in the pipeline's own shell.
    # shellcheck disable=SC2016
    if timeout --kill-after=10s "$attempt_timeout" bash -o pipefail -c '
      nix path-info --recursive "$1" | "$2" push --stdin --jobs 1 home
    ' bash "$result" "$attic"; then
      break
    fi
    if [ "$attempt" -eq "$attempts" ]; then
      echo "attic push of $result failed after $attempts attempts" >&2
      status=1
      break
    fi
    delay=$((attempt * 60 + RANDOM % 60))
    echo "attic push of $result failed (attempt $attempt/$attempts); retrying in ${delay}s"
    sleep "$delay"
  done
done

exit "$status"
